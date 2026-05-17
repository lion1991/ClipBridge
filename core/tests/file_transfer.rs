use std::fs;
use std::path::{Path, PathBuf};

use clipbridge_core::crypto::{sha256_hex, KEY_LEN};
use clipbridge_core::file_transfer::{
    sanitize_file_name, unique_destination_path, FileOffer, FileTransferConfig, FileTransferError,
    IncomingFileWriter, SendFileRequest,
};
use clipbridge_core::lan::{lan_send_file, receive_file_from_stream, send_file_to_stream};
use tokio::net::{TcpListener, TcpStream};

fn test_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "clipbridge-file-transfer-{label}-{}",
        uuid::Uuid::new_v4()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn assert_no_part_files(dir: &Path) {
    let entries = fs::read_dir(dir).unwrap();
    for entry in entries {
        let path = entry.unwrap().path();
        assert_ne!(
            path.extension().and_then(|s| s.to_str()),
            Some("part"),
            "left temporary file behind: {}",
            path.display()
        );
    }
}

#[test]
fn sanitizes_names_and_allocates_collision_names() {
    assert_eq!(sanitize_file_name("report.pdf").unwrap(), "report.pdf");

    assert!(matches!(
        sanitize_file_name("../secret.txt"),
        Err(FileTransferError::InvalidFileName { .. })
    ));
    assert!(matches!(
        sanitize_file_name("nested/report.pdf"),
        Err(FileTransferError::InvalidFileName { .. })
    ));
    assert!(matches!(
        sanitize_file_name("CON"),
        Err(FileTransferError::ReservedFileName { .. })
    ));

    let dir = test_dir("collisions");
    fs::write(dir.join("report.pdf"), b"old").unwrap();
    assert_eq!(
        unique_destination_path(&dir, "report.pdf")
            .unwrap()
            .file_name()
            .unwrap(),
        "report (1).pdf"
    );
    fs::write(dir.join("report (1).pdf"), b"old").unwrap();
    assert_eq!(
        unique_destination_path(&dir, "report.pdf")
            .unwrap()
            .file_name()
            .unwrap(),
        "report (2).pdf"
    );
}

#[test]
fn hash_mismatch_removes_part_file() {
    let dir = test_dir("hash-mismatch");
    let offer = FileOffer {
        transfer_id: "xfer-1".into(),
        source_device_id: "source".into(),
        target_device_id: "target".into(),
        file_name: "bad.bin".into(),
        size_bytes: 5,
        modified_unix_millis: None,
        mime_type: None,
        sha256_hex: sha256_hex(b"different"),
    };

    let mut writer =
        IncomingFileWriter::accept(offer, &dir, FileTransferConfig::default()).unwrap();
    writer.write_chunk(0, b"hello").unwrap();
    let err = writer.finish().unwrap_err();

    assert!(matches!(err, FileTransferError::HashMismatch { .. }));
    assert!(!dir.join("bad.bin").exists());
    assert_no_part_files(&dir);
}

#[tokio::test]
async fn lan_file_round_trip_writes_verified_file() {
    let root = test_dir("round-trip");
    let source = root.join("source.bin");
    let payload: Vec<u8> = (0..9000).map(|i| (i % 251) as u8).collect();
    fs::write(&source, &payload).unwrap();

    let destination = root.join("received");
    fs::create_dir_all(&destination).unwrap();

    let key = [13u8; KEY_LEN];
    let config = FileTransferConfig {
        chunk_bytes: 1024,
        ..FileTransferConfig::default()
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let receiver_config = config.clone();
    let receiver = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        receive_file_from_stream(
            stream,
            key,
            "target".into(),
            "Target".into(),
            destination,
            receiver_config,
        )
        .await
        .unwrap()
    });

    let stream = TcpStream::connect(addr).await.unwrap();
    let sent = send_file_to_stream(
        stream,
        key,
        SendFileRequest {
            source_device_id: "source".into(),
            source_device_name: "Source".into(),
            target_device_id: "target".into(),
            source_path: source,
            mime_type: None,
            config,
        },
    )
    .await
    .unwrap();
    let received = receiver.await.unwrap();

    assert_eq!(sent.bytes_sent, payload.len() as u64);
    assert_eq!(received.file_name, "source.bin");
    assert_eq!(received.size_bytes, payload.len() as u64);
    assert_eq!(received.sha256_hex, sha256_hex(&payload));
    assert_eq!(fs::read(&received.path).unwrap(), payload);
    assert_no_part_files(received.path.parent().unwrap());
}

#[tokio::test]
async fn lan_send_file_dials_candidates_and_transfers_file() {
    let root = test_dir("candidate-send");
    let source = root.join("from-candidates.txt");
    fs::write(&source, b"candidate path").unwrap();

    let destination = root.join("received");
    fs::create_dir_all(&destination).unwrap();

    let key = [21u8; KEY_LEN];
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let receiver = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        receive_file_from_stream(
            stream,
            key,
            "target".into(),
            "Target".into(),
            destination,
            FileTransferConfig::default(),
        )
        .await
        .unwrap()
    });

    let sent = tokio::task::spawn_blocking(move || {
        lan_send_file(
            vec![addr],
            key,
            SendFileRequest {
                source_device_id: "source".into(),
                source_device_name: "Source".into(),
                target_device_id: "target".into(),
                source_path: source,
                mime_type: Some("text/plain".into()),
                config: FileTransferConfig::default(),
            },
        )
    })
    .await
    .unwrap()
    .unwrap();
    let received = receiver.await.unwrap();

    assert_eq!(sent.bytes_sent, 14);
    assert_eq!(received.file_name, "from-candidates.txt");
    assert_eq!(fs::read_to_string(received.path).unwrap(), "candidate path");
}
