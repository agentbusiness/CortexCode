// CortexCode's notification app: a tiny, windowless (LSUIElement) app bundle that posts to macOS
// Notification Center through UNUserNotificationCenter and reports clicks back.
//
// UNUserNotificationCenter only works for a process that runs from an app bundle with a bundle
// identifier, which the `cc-notify` plugin binary is not. So the plugin starts this app's
// executable with `--serve` and talks to it over stdin/stdout, one JSON object per line:
//
//   -> {"op":"status","req":1}
//   <- {"req":1,"ok":true,"authorization":"authorized","alert":"enabled","bundle":"...","path":"..."}
//   -> {"op":"authorize","req":2}                 (shows the system's one-time permission prompt)
//   <- {"req":2,"ok":true,"granted":false,"authorization":"not_determined"}
//   -> {"op":"post","req":3,"id":"n1","title":"...","subtitle":"...","body":"...","thread":"...",
//       "actions":["Open"],"spool":"/path/activations.jsonl"}
//   <- {"req":3,"ok":true}  or  {"req":3,"ok":false,"error":"..."}
//   -> {"op":"delivered","req":4}                 <- {"req":4,"ok":true,"ids":["n1"]}
//   -> {"op":"remove","req":5,"ids":["n1"]}       <- {"req":5,"ok":true}
//   <- {"event":"activated","id":"n1","action":null,"ts_ms":...}   (the user clicked it)
//
// When the user clicks a notification while CortexCode is not running, macOS launches this app
// without `--serve`; it appends the activation to the spool file named in the notification and
// quits. The plugin drains the spool when it starts.

import AppKit
import Foundation
import UserNotifications

let serving = CommandLine.arguments.contains("--serve")
let center = UNUserNotificationCenter.current()
let outLock = NSLock()
var categories: [String: UNNotificationCategory] = [:]
let categoriesLock = NSLock()

func nowMs() -> Int64 {
    return Int64(Date().timeIntervalSince1970 * 1000)
}

func emit(_ object: [String: Any]) {
    guard let data = try? JSONSerialization.data(withJSONObject: object, options: []) else { return }
    outLock.lock()
    defer { outLock.unlock() }
    FileHandle.standardOutput.write(data)
    FileHandle.standardOutput.write(Data([0x0A]))
}

func authorizationName(_ status: UNAuthorizationStatus) -> String {
    switch status {
    case .notDetermined: return "not_determined"
    case .denied: return "denied"
    case .authorized: return "authorized"
    case .provisional: return "provisional"
    case .ephemeral: return "ephemeral"
    @unknown default: return "unknown"
    }
}

func settingName(_ setting: UNNotificationSetting) -> String {
    switch setting {
    case .notSupported: return "not_supported"
    case .disabled: return "disabled"
    case .enabled: return "enabled"
    @unknown default: return "unknown"
    }
}

/// Append one activation to the spool file (used when no plugin is listening).
func spool(_ record: [String: Any], to path: String) {
    guard let data = try? JSONSerialization.data(withJSONObject: record, options: []) else { return }
    let url = URL(fileURLWithPath: path)
    try? FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
    if !FileManager.default.fileExists(atPath: path) {
        FileManager.default.createFile(atPath: path, contents: nil)
    }
    guard let handle = try? FileHandle(forWritingTo: url) else { return }
    defer { try? handle.close() }
    _ = try? handle.seekToEnd()
    try? handle.write(contentsOf: data + Data([0x0A]))
}

final class Delegate: NSObject, UNUserNotificationCenterDelegate {
    // Present banners even when this app counts as frontmost.
    func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification,
                                withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void) {
        completionHandler([.banner, .list, .sound])
    }

    func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse,
                                withCompletionHandler completionHandler: @escaping () -> Void) {
        let request = response.notification.request
        var action: Any = NSNull()
        if response.actionIdentifier.hasPrefix("act:") {
            action = String(response.actionIdentifier.dropFirst(4))
        } else if response.actionIdentifier == UNNotificationDismissActionIdentifier {
            completionHandler()
            return
        }
        let record: [String: Any] = ["event": "activated", "id": request.identifier, "action": action, "ts_ms": nowMs()]
        if serving {
            emit(record)
        } else if let path = request.content.userInfo["spool"] as? String, !path.isEmpty {
            spool(record, to: path)
        }
        completionHandler()
        if !serving {
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { NSApp.terminate(nil) }
        }
    }
}

func reply(_ req: Any, _ fields: [String: Any]) {
    var object = fields
    object["req"] = req
    emit(object)
}

func category(for actions: [String]) -> String? {
    if actions.isEmpty { return nil }
    let id = "cc.actions." + actions.joined(separator: "\u{1F}")
    categoriesLock.lock()
    let known = categories[id] != nil
    if !known {
        let buttons = actions.map { UNNotificationAction(identifier: "act:" + $0, title: $0, options: [.foreground]) }
        categories[id] = UNNotificationCategory(identifier: id, actions: buttons, intentIdentifiers: [], options: [])
    }
    let all = Set(categories.values)
    categoriesLock.unlock()
    if !known {
        center.setNotificationCategories(all)
    }
    return id
}

func handle(_ line: String) {
    guard let data = line.data(using: .utf8),
          let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
          let op = object["op"] as? String else {
        emit(["event": "error", "error": "unreadable command"])
        return
    }
    let req: Any = object["req"] ?? NSNull()
    switch op {
    case "status":
        center.getNotificationSettings { settings in
            reply(req, ["ok": true,
                        "authorization": authorizationName(settings.authorizationStatus),
                        "alert": settingName(settings.alertSetting),
                        "center": settingName(settings.notificationCenterSetting),
                        "bundle": Bundle.main.bundleIdentifier ?? "",
                        "path": Bundle.main.bundlePath])
        }
    case "authorize":
        center.requestAuthorization(options: [.alert, .sound, .badge]) { granted, error in
            center.getNotificationSettings { settings in
                var fields: [String: Any] = ["ok": true, "granted": granted,
                                             "authorization": authorizationName(settings.authorizationStatus)]
                if let error = error { fields["error"] = error.localizedDescription }
                reply(req, fields)
            }
        }
    case "post":
        guard let id = object["id"] as? String, let title = object["title"] as? String else {
            reply(req, ["ok": false, "error": "post needs id and title"])
            return
        }
        let content = UNMutableNotificationContent()
        content.title = title
        if let subtitle = object["subtitle"] as? String { content.subtitle = subtitle }
        if let body = object["body"] as? String { content.body = body }
        if let thread = object["thread"] as? String { content.threadIdentifier = thread }
        content.sound = UNNotificationSound.default
        var info: [String: Any] = ["cc_id": id]
        if let path = object["spool"] as? String { info["spool"] = path }
        content.userInfo = info
        if let cat = category(for: (object["actions"] as? [String]) ?? []) {
            content.categoryIdentifier = cat
        }
        let request = UNNotificationRequest(identifier: id, content: content, trigger: nil)
        center.add(request) { error in
            if let error = error {
                reply(req, ["ok": false, "error": error.localizedDescription, "code": (error as NSError).code,
                            "domain": (error as NSError).domain])
            } else {
                reply(req, ["ok": true])
            }
        }
    case "delivered":
        center.getDeliveredNotifications { notifications in
            reply(req, ["ok": true, "ids": notifications.map { $0.request.identifier }])
        }
    case "remove":
        let ids = (object["ids"] as? [String]) ?? []
        center.removeDeliveredNotifications(withIdentifiers: ids)
        reply(req, ["ok": true])
    default:
        reply(req, ["ok": false, "error": "unknown op \(op)"])
    }
}

let app = NSApplication.shared
app.setActivationPolicy(.accessory)
let delegate = Delegate()
center.delegate = delegate

if serving {
    // Commands arrive on stdin; the plugin closing the pipe ends the app.
    Thread.detachNewThread {
        while let line = readLine(strippingNewline: true) {
            if line.isEmpty { continue }
            handle(line)
        }
        // Let answers to the last commands go out before quitting.
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.0) { exit(0) }
    }
    emit(["event": "ready", "bundle": Bundle.main.bundleIdentifier ?? "", "path": Bundle.main.bundlePath,
          "pid": Int(getpid())])
} else {
    // Launched by macOS for a click while nothing was listening: take the response, then quit.
    DispatchQueue.main.asyncAfter(deadline: .now() + 15) { NSApp.terminate(nil) }
}
app.run()
