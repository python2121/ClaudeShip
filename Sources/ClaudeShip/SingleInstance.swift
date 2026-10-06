import Foundation

/// Guards the GUI path against duplicate menubar items. macOS only de-dupes
/// `.app` launches via LaunchServices, so running the raw binary inside the
/// bundle (or `open`ing the app while the LaunchAgent copy is up) would stack
/// a second status item. A POSIX flock is held for the process lifetime and
/// released by the kernel on exit, so it can't go stale.
enum SingleInstance {
    private static var lockFD: Int32 = -1

    static func acquire() -> Bool {
        let dir = FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Application Support/ClaudeShip", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let path = dir.appendingPathComponent("instance.lock").path

        let fd = open(path, O_CREAT | O_RDWR, 0o600)
        guard fd >= 0 else { return true }  // can't lock — don't block launch
        if flock(fd, LOCK_EX | LOCK_NB) != 0 {
            close(fd)
            return false
        }
        lockFD = fd  // keep it open for the process lifetime
        return true
    }
}
