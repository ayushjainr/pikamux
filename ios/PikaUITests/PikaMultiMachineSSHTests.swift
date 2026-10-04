import Foundation
import XCTest

/// Actual app/Keychain/ordinary OpenSSH, with explicitly labeled protocol doubles.
/// This is routing evidence, not Rust endpoint/provider/fleet evidence.
final class PikaMultiMachineSSHTests: XCTestCase {
    struct Machine: Decodable { let nodeId: String; let name: String; let port: Int; let fingerprint: String }
    struct Configuration: Decodable {
        let root: String; let clientKeyPath: String; let username: String; let storeId: String; let machines: [Machine]
    }
    @MainActor func testRetainedDisposableStoreSettledCardFreshness() throws {
        continueAfterFailure = false
        guard let path = ProcessInfo.processInfo.environment["PIKA_MULTI_SSH_TEST_CONFIG"], !path.isEmpty, !path.contains("$(") else {
            throw XCTSkip("Requires explicitly retained disposable machine store.")
        }
        let config = try JSONDecoder().decode(Configuration.self, from: Data(contentsOf: URL(fileURLWithPath: path)))
        XCTAssertTrue(FileManager.default.fileExists(atPath: config.root + "/offline-beta"))
        let app = XCUIApplication()
        app.launchArguments = ["--ssh-integration-test", "--multi-machine-integration-test"]
        app.launchEnvironment = ["PIKA_UI_TEST_KEY_BASE64": try Data(contentsOf: URL(fileURLWithPath: config.clientKeyPath)).base64EncodedString(), "PIKA_UI_TEST_STORE_ID": config.storeId]
        app.launch()
        XCTAssertTrue(app.buttons["Machine connections"].waitForExistence(timeout: 5)); app.buttons["Machine connections"].tap()
        for index in [0, 2] {
            XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: NSPredicate(format: "label CONTAINS[c] %@", "connected"), object: app.staticTexts["machineStatus-" + config.machines[index].nodeId])], timeout: 25), .completed)
        }
        XCTAssertTrue(app.staticTexts["machineStatus-" + config.machines[1].nodeId].label.localizedCaseInsensitiveContains("offline"))
        app.buttons["Cancel"].tap()
        for (index, machine) in config.machines.enumerated() {
            let key = [machine.nodeId, "codex", "collision-thread"].map { Data($0.utf8).base64EncodedString() }.joined(separator: ":")
            let observation = app.staticTexts["boardFreshness-" + key]
            XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: NSPredicate(format: "label BEGINSWITH %@", index == 1 ? "Cached" : "Observed"), object: observation)], timeout: 10), .completed, "Actual rendered row text must match its exact connection, not a reused cached card")
            XCTAssertTrue(app.buttons["thread-" + key].exists)
        }
        let shot = XCTAttachment(screenshot: app.screenshot()); shot.name = "Settled native Dex: rs6 and Gamma Observed, only Beta Cached"; shot.lifetime = .keepAlways; add(shot)
    }
    /// Explicit follow-up against ONLY the retained disposable store from the
    /// completed onboarding journey. Not a general user's saved connections.
    @MainActor func testRetainedDisposableStoreOfflineAndExactRouting() throws {
        continueAfterFailure = false
        guard let path = ProcessInfo.processInfo.environment["PIKA_MULTI_SSH_TEST_CONFIG"], !path.isEmpty, !path.contains("$(") else {
            throw XCTSkip("Requires the explicitly retained disposable three-endpoint fixture.")
        }
        let config = try JSONDecoder().decode(Configuration.self, from: Data(contentsOf: URL(fileURLWithPath: path)))
        guard FileManager.default.fileExists(atPath: config.root + "/offline-beta") else {
            throw XCTSkip("Run onboarding journey first; this follows only its retained offline fixture.")
        }
        func records(_ name: String) throws -> [[String: Any]] {
            try String(contentsOfFile: config.root + "/" + name + ".jsonl", encoding: .utf8).split(separator: "\n").map {
                try JSONSerialization.jsonObject(with: Data($0.utf8)) as! [String: Any]
            }
        }
        let counts = try Dictionary(uniqueKeysWithValues: config.machines.map { ($0.name, try records($0.name).filter { $0["method"] as? String == "conversation/send" }.count) })
        let app = XCUIApplication()
        app.launchArguments = ["--ssh-integration-test", "--multi-machine-integration-test"]
        app.launchEnvironment = ["PIKA_UI_TEST_KEY_BASE64": try Data(contentsOf: URL(fileURLWithPath: config.clientKeyPath)).base64EncodedString(), "PIKA_UI_TEST_STORE_ID": config.storeId]
        app.launch()
        func row(_ machine: Machine) -> XCUIElement {
            app.buttons["thread-" + [machine.nodeId, "codex", "collision-thread"].map { Data($0.utf8).base64EncodedString() }.joined(separator: ":")]
        }
        for machine in config.machines { XCTAssertTrue(row(machine).waitForExistence(timeout: 10)) }
        XCTAssertTrue(row(config.machines[0]).label.contains("rs6"))
        app.buttons["Machine connections"].tap()
        for index in [0, 2] {
            XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: NSPredicate(format: "label CONTAINS[c] %@", "connected"), object: app.staticTexts["machineStatus-" + config.machines[index].nodeId])], timeout: 25), .completed)
        }
        XCTAssertTrue(app.staticTexts["machineStatus-" + config.machines[1].nodeId].label.localizedCaseInsensitiveContains("offline"))
        let states = XCTAttachment(screenshot: app.screenshot()); states.name = "Frozen source retained rs6 and Gamma online, Beta offline"; states.lifetime = .keepAlways; add(states)
        app.buttons["Cancel"].tap()
        for (index, machine) in config.machines.enumerated() {
            let key = [machine.nodeId, "codex", "collision-thread"].map { Data($0.utf8).base64EncodedString() }.joined(separator: ":")
            XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: NSPredicate(format: "label BEGINSWITH %@", index == 1 ? "Cached" : "Observed"), object: app.staticTexts["boardFreshness-" + key])], timeout: 10), .completed, "The exact rendered card must reflect its own node's live/cached state")
        }
        let board = XCTAttachment(screenshot: app.screenshot()); board.name = "Frozen source native Dex with neutral Cached Beta and retained rs6"; board.lifetime = .keepAlways; add(board)
        try Data("hold".utf8).write(to: URL(fileURLWithPath: config.root).appendingPathComponent("alpha-open-hold"), options: .atomic)
        row(config.machines[0]).tap()
        func waitForMarker(_ name: String) {
            let predicate = NSPredicate { _, _ in FileManager.default.fileExists(atPath: config.root + "/" + name) }
            XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: predicate, object: nil)], timeout: 20), .completed)
        }
        waitForMarker("alpha-open-held")
        try Data("disconnect".utf8).write(to: URL(fileURLWithPath: config.root).appendingPathComponent("gamma-disconnect-once"), options: .atomic)
        waitForMarker("gamma-reconnected")
        try Data("release".utf8).write(to: URL(fileURLWithPath: config.root).appendingPathComponent("alpha-open-release"), options: .atomic)
        XCTAssertTrue(app.staticTexts["Protocol fixture context from Alpha. No provider attached."].waitForExistence(timeout: 15))
        try Data("inject".utf8).write(to: URL(fileURLWithPath: config.root).appendingPathComponent("gamma-source-error"), options: .atomic)
        waitForMarker("gamma-source-error-delivered")
        let composer = app.textViews["composer"]; composer.tap(); composer.typeText("Exact destination Alpha")
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: NSPredicate(format: "enabled == true"), object: app.buttons["sendReply"])], timeout: 10), .completed)
        app.buttons["sendReply"].tap()
        XCTAssertTrue(app.staticTexts["Protocol fixture received exact destination Alpha"].waitForExistence(timeout: 15))
        let reply = XCTAttachment(screenshot: app.screenshot()); reply.name = "Frozen source exact Alpha reply after unrelated Gamma reconnect and error"; reply.lifetime = .keepAlways; add(reply)
        for machine in config.machines {
            let sends = try records(machine.name).filter { $0["method"] as? String == "conversation/send" }
            XCTAssertEqual(sends.count, (counts[machine.name] ?? 0) + (machine.name == "Alpha" ? 1 : 0), "Only exact Alpha receives the new send; no offline replay")
            for send in sends {
                let identity = (send["params"] as! [String: Any])["identity"] as! [String: Any]
                XCTAssertEqual(identity["nodeId"] as? String, machine.nodeId)
            }
        }
    }
    @MainActor func testThreeSavedMachinesCollisionRoutingAndIndependentOffline() throws {
        continueAfterFailure = false
        guard let path = ProcessInfo.processInfo.environment["PIKA_MULTI_SSH_TEST_CONFIG"], !path.isEmpty, !path.contains("$(") else {
            throw XCTSkip("Requires explicitly scoped disposable three-endpoint SSH fixture.")
        }
        let config = try JSONDecoder().decode(Configuration.self, from: Data(contentsOf: URL(fileURLWithPath: path)))
        XCTAssertEqual(config.machines.count, 3)
        XCTAssertEqual(Set(config.machines.map(\.nodeId)).count, 3)
        let app = XCUIApplication()
        app.launchArguments = ["--ssh-integration-test", "--multi-machine-integration-test"]
        app.launchEnvironment = ["PIKA_UI_TEST_KEY_BASE64": try Data(contentsOf: URL(fileURLWithPath: config.clientKeyPath)).base64EncodedString(), "PIKA_UI_TEST_STORE_ID": config.storeId]
        app.launch()
        func row(_ machine: Machine) -> XCUIElement {
            let key = [machine.nodeId, "codex", "collision-thread"].map { Data($0.utf8).base64EncodedString() }.joined(separator: ":")
            return app.buttons["thread-" + key]
        }
        func capture(_ name: String) {
            let attachment = XCTAttachment(screenshot: app.screenshot()); attachment.name = name; attachment.lifetime = .keepAlways; add(attachment)
        }
        func back() {
            app.coordinate(withNormalizedOffset: CGVector(dx: 0.01, dy: 0.4)).press(forDuration: 0.1, thenDragTo: app.coordinate(withNormalizedOffset: CGVector(dx: 0.85, dy: 0.4)))
        }
        for machine in config.machines {
            if app.buttons["Add a machine"].exists { app.buttons["Add a machine"].tap() }
            else { app.buttons["Machine connections"].tap() }
            app.buttons["Manual login"].tap()
            let address = app.textFields["machineAddress"]
            XCTAssertTrue(address.waitForExistence(timeout: 5)); address.tap(); address.typeText("127.0.0.1")
            app.textFields["Username"].tap(); app.textFields["Username"].typeText(config.username)
            let port = app.textFields["SSH port"]; port.tap(); port.typeText(String(repeating: XCUIKeyboardKey.delete.rawValue, count: 2)); port.typeText(String(machine.port))
            app.buttons["Import key"].tap(); app.buttons["importIntegrationKey"].tap()
            for _ in 0..<4 where !app.buttons["connectMachine"].isHittable { app.swipeUp() }
            app.buttons["connectMachine"].tap()
            XCTAssertTrue(app.staticTexts[machine.fingerprint].waitForExistence(timeout: 15))
            app.buttons["I verified this fingerprint"].tap()
            XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: NSPredicate(format: "exists == false"), object: app.buttons["connectMachine"])], timeout: 20), .completed)
            XCTAssertTrue(row(machine).waitForExistence(timeout: 20))
        }
        for machine in config.machines { XCTAssertTrue(row(machine).exists); XCTAssertTrue(row(machine).label.contains("SSH fixture " + machine.name)) }
        capture("Three separately pinned SSH endpoints with colliding provider UUID and name")
        app.terminate(); app.launch()
        for machine in config.machines { XCTAssertTrue(row(machine).waitForExistence(timeout: 25)) }
        app.buttons["Machine connections"].tap()
        for machine in config.machines {
            XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: NSPredicate(format: "label CONTAINS[c] %@", "connected"), object: app.staticTexts["machineStatus-" + machine.nodeId])], timeout: 25), .completed)
        }
        app.buttons["Cancel"].tap()
        XCTAssertFalse(app.buttons["I verified this fingerprint"].exists)
        capture("Three saved Keychain logins restored after process death")
        let filter = app.buttons["machineFilter"]
        XCTAssertTrue(filter.waitForExistence(timeout: 5)); filter.tap()
        app.buttons["SSH fixture Alpha"].tap()
        XCTAssertTrue(row(config.machines[0]).exists); XCTAssertFalse(row(config.machines[1]).exists); XCTAssertFalse(row(config.machines[2]).exists)
        filter.tap(); app.buttons["All machines"].tap()
        // Last onboarding was Gamma. Alpha must still receive the exact open/send.
        for index in [0, 2] {
            let machine = config.machines[index]
            row(machine).tap()
            XCTAssertTrue(app.staticTexts["Protocol fixture context from " + machine.name + ". No provider attached."].waitForExistence(timeout: 20))
            let composer = app.textViews["composer"]; composer.tap(); composer.typeText("Exact destination " + machine.name)
            XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: NSPredicate(format: "enabled == true"), object: app.buttons["sendReply"])], timeout: 10), .completed)
            app.buttons["sendReply"].tap()
            XCTAssertTrue(app.staticTexts["Protocol fixture received exact destination " + machine.name].waitForExistence(timeout: 15))
            capture("Exact colliding conversation reply on " + machine.name)
            back()
        }
        app.buttons["Machine connections"].tap()
        let rename = app.buttons["renameMachine-" + config.machines[0].nodeId]
        XCTAssertTrue(rename.waitForExistence(timeout: 5)); rename.tap()
        // UIKit's native alert text field drops SwiftUI's identifier; target its
        // explicit placeholder within the verified native nickname alert.
        let nickname = app.alerts["Machine nickname"].textFields["Nickname (optional)"]
        XCTAssertTrue(nickname.waitForExistence(timeout: 5)); nickname.tap(); nickname.typeText("rs6")
        app.buttons["Save"].tap(); app.buttons["Cancel"].tap()
        XCTAssertTrue(row(config.machines[0]).label.contains("rs6"))
        app.terminate(); app.launch()
        XCTAssertTrue(row(config.machines[0]).waitForExistence(timeout: 15)); XCTAssertTrue(row(config.machines[0]).label.contains("rs6"))
        app.buttons["Machine connections"].tap()
        for machine in config.machines {
            XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: NSPredicate(format: "label CONTAINS[c] %@", "connected"), object: app.staticTexts["machineStatus-" + machine.nodeId])], timeout: 25), .completed)
        }
        app.buttons["Cancel"].tap()
        capture("Persisted rs6 nickname on three-machine Dex")
        app.buttons["pikaTab"].tap()
        let assistantMachine = app.buttons["assistantMachine"]
        XCTAssertTrue(assistantMachine.waitForExistence(timeout: 5))
        for name in ["rs6", "SSH fixture Beta", "SSH fixture Gamma"] {
            assistantMachine.tap(); app.buttons[name].tap()
            XCTAssertTrue((assistantMachine.label + " " + (assistantMachine.value as? String ?? "")).contains(name))
            XCTAssertTrue(app.buttons["Open Pika"].exists)
        }
        capture("Saved rs6 nickname and explicit three-machine Pika chooser")
        app.buttons["dexTab"].tap()
        try Data("offline".utf8).write(to: URL(fileURLWithPath: config.root).appendingPathComponent("offline-beta"), options: .atomic)
        app.buttons["Machine connections"].tap()
        let offline = app.staticTexts["machineStatus-" + config.machines[1].nodeId]
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: NSPredicate(format: "label CONTAINS[c] %@", "offline"), object: offline)], timeout: 20), .completed)
        for index in [0, 2] { XCTAssertTrue(app.staticTexts["machineStatus-" + config.machines[index].nodeId].label.localizedCaseInsensitiveContains("connected")) }
        capture("Beta offline while Alpha and Gamma remain connected")
        app.buttons["Cancel"].tap()
        for machine in config.machines { XCTAssertTrue(row(machine).exists, "Offline cached row and the two live rows must survive") }
        row(config.machines[0]).tap()
        XCTAssertTrue(app.staticTexts["Protocol fixture context from Alpha. No provider attached."].waitForExistence(timeout: 15))
        XCTAssertTrue(app.textViews["composer"].exists)
        // Read server-owned logs, independently of UI receipts, after every send.
        for machine in config.machines {
            let lines = try String(contentsOfFile: config.root + "/" + machine.name + ".jsonl", encoding: .utf8).split(separator: "\n")
            let records = try lines.map { try JSONSerialization.jsonObject(with: Data($0.utf8)) as! [String: Any] }
            let sends = records.filter { $0["method"] as? String == "conversation/send" }
            XCTAssertEqual(sends.count, machine.name == "Beta" ? 0 : 1, "No send retry or cross-machine dispatch")
            for record in records where ["conversation/open", "conversation/send"].contains(record["method"] as? String ?? "") {
                let params = record["params"] as! [String: Any]; let identity = params["identity"] as! [String: Any]
                XCTAssertEqual(identity["nodeId"] as? String, machine.nodeId)
                XCTAssertEqual(identity["provider"] as? String, "codex")
                XCTAssertEqual(identity["threadId"] as? String, "collision-thread")
            }
            if let send = sends.first { XCTAssertEqual((send["params"] as! [String: Any])["text"] as? String, "Exact destination " + machine.name) }
        }
    }
}
