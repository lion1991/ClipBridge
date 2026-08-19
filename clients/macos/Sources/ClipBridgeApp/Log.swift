import os

/// Unified-log channels for the menu-bar app.
///
/// There is no window and no console here, so when the transport wedged the
/// only evidence was the menu label itself — nothing to look at after the
/// fact. Everything that moves the connection (core state transitions, wake,
/// network changes, watchdog actions) writes here at `.notice`, which is the
/// lowest level the log store keeps on disk.
///
/// Read it back with:
///   log show --last 2h --predicate 'subsystem == "com.clipbridge.mac"'
enum Log {
    static let connection = Logger(subsystem: "com.clipbridge.mac", category: "connection")
}
