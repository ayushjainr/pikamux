import Foundation
import CryptoKit
import XCTest

/// Installed app -> ordinary OpenSSH -> real Rust mobile endpoint -> real
/// installed Codex with a synthetic model only. Explicit fixture config required.
final class PikaSSHIntegrationTests: XCTestCase {
    struct Configuration: Decodable {
        let address: String
        let port: Int
        let username: String
        let clientKeyPath: String
        let fingerprint: String
        let threadId: String
        let threadName: String
        let expectedContext: String
        let adoptReadyPath: String?
        let adoptRemovedPath: String?
        let mode: String?
        let finalResponse: String?
        let projectName: String?
        let creationName: String?
        let createdIdentityPath: String?
        let reply: String?
        let storeId: String?
        let beforeCreationReadyPath: String?
        let beforeCreationProceedPath: String?
        let approvalCommand: String?
        let approvalReason: String?
        let expectedCodeSha256: String?
        let expectedLargeUserSha256: String?
        let expectedLargeReplySha256: String?
    }
    @MainActor func testManualSSHOnboardingReadAndReplyToExactExistingConversation() throws {
        continueAfterFailure = false
        guard let path = ProcessInfo.processInfo.environment["PIKA_SSH_TEST_CONFIG"], !path.isEmpty, !path.contains("$(") else {
            throw XCTSkip("No explicitly scoped disposable SSH/Codex integration fixture configured.")
        }
        let config = try JSONDecoder().decode(Configuration.self, from: Data(contentsOf: URL(fileURLWithPath: path)))
        let key = try Data(contentsOf: URL(fileURLWithPath: config.clientKeyPath))
        let app = XCUIApplication()
        app.launchArguments = ["--ssh-integration-test"]
        app.launchEnvironment = ["PIKA_UI_TEST_KEY_BASE64": key.base64EncodedString(), "PIKA_UI_TEST_STORE_ID": config.storeId ?? UUID().uuidString]
        app.launch()
        XCTAssertTrue(app.staticTexts["sshIntegrationBanner"].waitForExistence(timeout: 5))
        if config.mode == "adopt" {
            XCTAssertNotNil(UUID(uuidString: config.storeId ?? ""), "Adoption must retain the explicitly scoped original saved login")
            let connected = NSPredicate(format: "label BEGINSWITH %@ OR label BEGINSWITH %@", "Observed", "Connected")
            XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: connected, object: app.staticTexts["connectionStatus"])], timeout: 20), .completed)
            XCTAssertFalse(app.buttons["I verified this fingerprint"].exists)
            app.buttons["addThread"].tap(); app.buttons["Add an existing thread"].tap()
            let candidate = app.buttons["candidate-" + config.threadId]
            XCTAssertTrue(candidate.waitForExistence(timeout: 20)); candidate.tap()
            XCTAssertTrue(app.staticTexts[config.expectedContext].waitForExistence(timeout: 20))
            if let response = config.finalResponse {
                XCTAssertTrue(app.staticTexts[response].waitForExistence(timeout: 20))
            }
            let screenshot = XCTAttachment(screenshot: app.screenshot()); screenshot.name = "Actual original thread adopted with saved login"; screenshot.lifetime = .keepAlways; add(screenshot)
            return
        }
        app.buttons["Add a machine"].tap()
        app.buttons["Manual login"].tap()
        let address = app.textFields["machineAddress"]
        XCTAssertTrue(address.waitForExistence(timeout: 5))
        address.tap(); address.typeText(config.address)
        app.textFields["Username"].tap(); app.textFields["Username"].typeText(config.username)
        let port = app.textFields["SSH port"]
        port.tap(); port.typeText(String(repeating: XCUIKeyboardKey.delete.rawValue, count: 2)); port.typeText(String(config.port))
        app.buttons["Import key"].tap()
        app.buttons["importIntegrationKey"].tap()
        for _ in 0..<3 where !app.buttons["connectMachine"].exists { app.swipeUp() }
        app.buttons["connectMachine"].tap()
        let fingerprint = app.staticTexts[config.fingerprint]
        XCTAssertTrue(fingerprint.waitForExistence(timeout: 15))
        app.buttons["I verified this fingerprint"].tap()
        let row = app.buttons["thread-" + config.threadId]
        if config.mode == "assistant" || config.mode == "create" || config.mode == "adoptFresh" || config.mode == "approval" {
            XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: NSPredicate(format: "exists == false"), object: app.buttons["connectMachine"])], timeout: 20), .completed)
            let connected = NSPredicate(format: "label BEGINSWITH %@ OR label BEGINSWITH %@", "Observed", "Connected")
            XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: connected, object: app.staticTexts["connectionStatus"])], timeout: 20), .completed, "Validated board connection must finish saving before process death")
            app.terminate(); app.launch()
            XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: connected, object: app.staticTexts["connectionStatus"])], timeout: 20), .completed)
            if config.mode == "approval" {
                XCTAssertTrue(row.waitForExistence(timeout: 20)); row.tap()
                XCTAssertTrue(app.staticTexts[config.expectedContext].waitForExistence(timeout: 20))
                guard let command = config.approvalCommand, let reason = config.approvalReason else { XCTFail("Original native approval details are required"); return }
                XCTAssertTrue(app.staticTexts[command].waitForExistence(timeout: 10))
                XCTAssertTrue(app.staticTexts[reason].exists)
                let allow = app.buttons["acceptProviderRequest"]
                XCTAssertTrue(allow.waitForExistence(timeout: 10)); XCTAssertTrue(allow.isEnabled)
                let originalRequest = XCTAttachment(screenshot: app.screenshot()); originalRequest.name = "Actual original command and offered Allow once"; originalRequest.lifetime = .keepAlways; add(originalRequest)
                allow.tap()
                XCTAssertFalse(allow.exists && allow.isEnabled, "The exact original decision cannot be submitted twice; a resolved request may already be absent")
                XCTAssertTrue(app.staticTexts[config.finalResponse ?? "Approval journey finished."].waitForExistence(timeout: 30))
                let screenshot = XCTAttachment(screenshot: app.screenshot()); screenshot.name = "Actual original command once approval"; screenshot.lifetime = .keepAlways; add(screenshot)
                return
            } else if config.mode == "adoptFresh" {
                app.buttons["addThread"].tap(); app.buttons["Add an existing thread"].tap()
                let candidate = app.buttons["candidate-" + config.threadId]
                XCTAssertTrue(candidate.waitForExistence(timeout: 20)); candidate.tap()
                XCTAssertTrue(app.staticTexts[config.expectedContext].waitForExistence(timeout: 20))
                if let response = config.finalResponse { XCTAssertTrue(app.staticTexts[response].waitForExistence(timeout: 20)) }
                let screenshot = XCTAttachment(screenshot: app.screenshot()); screenshot.name = "Actual existing native UUID admitted without replacement"; screenshot.lifetime = .keepAlways; add(screenshot)
                return
            } else if config.mode == "assistant" {
                app.buttons["pikaTab"].tap(); app.buttons["Open Pika"].tap()
            } else {
                if let ready = config.beforeCreationReadyPath, let proceed = config.beforeCreationProceedPath {
                    try Data("ready".utf8).write(to: URL(fileURLWithPath: ready), options: .atomic)
                    let released = NSPredicate { _, _ in FileManager.default.fileExists(atPath: proceed) }
                    XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: released, object: nil)], timeout: 45), .completed)
                }
                // These real read-only selectors run after the optional host
                // listener pause, proving the authenticated channel remains live.
                app.buttons["addThread"].tap(); app.buttons["Start a thread"].tap()
                let name = app.textFields["newThreadName"]
                XCTAssertTrue(name.waitForExistence(timeout: 10))
                name.tap(); name.typeText((config.creationName ?? "Native SSH creation") + "\n")
                if let project = config.projectName {
                    app.buttons["threadProjectPicker"].tap(); app.buttons[project].tap()
                }
                app.buttons["startThread"].tap()
            }
            let composer = app.textViews["composer"]
            XCTAssertTrue(composer.waitForExistence(timeout: 20))
            if let marker = config.createdIdentityPath {
                let conversation = app.scrollViews.matching(NSPredicate(format: "identifier BEGINSWITH %@", "conversation-")).firstMatch
                XCTAssertTrue(conversation.waitForExistence(timeout: 10))
                let thread = String(conversation.identifier.dropFirst("conversation-".count))
                XCTAssertFalse(thread.isEmpty)
                try Data(thread.utf8).write(to: URL(fileURLWithPath: marker), options: .atomic)
            }
            composer.tap(); composer.typeText(config.reply ?? "Keep my synthetic preference")
            XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: NSPredicate(format: "enabled == true"), object: app.buttons["sendReply"])], timeout: 15), .completed)
            app.buttons["sendReply"].tap()
            XCTAssertTrue(app.staticTexts[config.finalResponse ?? "Original assistant received the exact mobile reply."].waitForExistence(timeout: 30))
            let screenshot = XCTAttachment(screenshot: app.screenshot()); screenshot.name = "Actual saved Pika assistant original mobile reply"; screenshot.lifetime = .keepAlways; add(screenshot)
            return
        }
        XCTAssertTrue(row.waitForExistence(timeout: 20), "Expected exact pre-existing thread on the permitted board")
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: NSPredicate(format: "exists == false"),
            object: app.buttons["connectMachine"])], timeout: 15), .completed, "Onboarding must complete before process termination")
        // Keep the same scoped Keychain/store identifier across process death.
        app.terminate(); app.launch()
        XCTAssertTrue(row.waitForExistence(timeout: 20), "Saved login must reconnect without a repeated host challenge")
        XCTAssertFalse(app.buttons["I verified this fingerprint"].exists)
        row.tap()
        if !config.expectedContext.isEmpty {
            XCTAssertTrue(app.staticTexts[config.expectedContext].waitForExistence(timeout: 15))
        }
        if config.mode == "largeHistory" {
            let latest = app.staticTexts["Synthetic large Claude assistant 119"]
            XCTAssertTrue(latest.waitForExistence(timeout: 20))
            XCTAssertTrue(latest.isHittable, "Opening a large saved history must show its latest reply")
            XCTAssertFalse(app.buttons["sendReply"].isEnabled, "Synthetic source has no native owner or send route")
            let conversation = app.scrollViews["conversation-" + config.threadId]
            XCTAssertTrue(conversation.waitForExistence(timeout: 10))
            func revealReaderControl(_ control: XCUIElement, towardsOlder: Bool) {
                for _ in 0..<64 {
                    // Native accessibility can call an offscreen descendant
                    // hittable even when the header clips its actual button.
                    // Require the whole control inside the visible viewport.
                    let top = max(conversation.frame.minY, app.frame.minY + app.frame.height * 0.15)
                    let bottom = min(conversation.frame.maxY, app.frame.maxY - app.frame.height * 0.15)
                    let frame = control.frame
                    if control.isHittable && frame.minY >= top && frame.maxY <= bottom { return }
                    // Drag in the transcript's outer padding, not inside the
                    // native reader. Fixture chronology supplies direction;
                    // offscreen accessibility frames can have stale positions.
                    // Small drags cannot skip the whole visible control band.
                    let start = conversation.coordinate(withNormalizedOffset: CGVector(dx: 0.99, dy: towardsOlder ? 0.3 : 0.65))
                    let end = conversation.coordinate(withNormalizedOffset: CGVector(dx: 0.99, dy: towardsOlder ? 0.55 : 0.4))
                    start.press(forDuration: 0.1, thenDragTo: end)
                }
                XCTFail("Native reader control must become fully visible within finite real gestures")
            }
            for (identifier, expectedHash) in [("large-reply-content", config.expectedLargeReplySha256), ("large-user-content", config.expectedLargeUserSha256), ("large-code-content", config.expectedCodeSha256)] {
                if let expectedHash {
                    let reader = app.textViews[identifier]
                    XCTAssertTrue(reader.waitForExistence(timeout: 10), "The full original large message must be accessible through its native reader")
                    var original = ""
                    var partCount = 0
                    while true {
                        let visiblePart = try XCTUnwrap(reader.value as? String)
                        XCTAssertLessThanOrEqual(visiblePart.utf8.count, 16 * 1024, "Native layout work must remain bounded even for long combining-character graphemes")
                        original += visiblePart
                        partCount += 1
                        guard partCount < 512 else {
                            XCTFail("Native parts must make finite progress without repeating a page")
                            break
                        }
                        let next = app.buttons[identifier + "-next"]
                        if !next.exists || !next.isEnabled { break }
                        revealReaderControl(next, towardsOlder: identifier != "large-code-content")
                        XCTAssertTrue(next.isHittable, "Every original text part must remain reachable")
                        let indicator = app.staticTexts[identifier + "-page"]
                        let previousIndicator = indicator.label
                        next.tap()
                        XCTAssertNotEqual(indicator.label, previousIndicator, "Next must advance the native part indicator")
                    }
                    let originalHash = SHA256.hash(data: Data(original.utf8)).map { String(format: "%02x", $0) }.joined()
                    XCTAssertEqual(originalHash, expectedHash, "Native reader must preserve the entire original message, including Unicode, escapes and Markdown fences")
                }
            }
            let older = app.buttons["Load older context"]
            if let expectedCodeSha256 = config.expectedCodeSha256 {
                let copy = app.buttons["Copy code"]
                revealReaderControl(copy, towardsOlder: true)
                XCTAssertTrue(copy.isHittable, "The actual large assistant code block must render with native Copy")
                copy.tap()
                XCTAssertTrue(app.buttons["Code copied"].exists)
                let composer = app.textViews["composer"]
                composer.press(forDuration: 1.2)
                let paste = app.menuItems["Paste"]
                if paste.waitForExistence(timeout: 3) { paste.tap() }
                else { app.buttons["Paste"].tap() }
                let copied = try XCTUnwrap(composer.value as? String)
                let copiedSha256 = SHA256.hash(data: Data(copied.utf8)).map { String(format: "%02x", $0) }.joined()
                XCTAssertEqual(copiedSha256, expectedCodeSha256, "Native Copy must preserve every byte of the >256 KiB code body")
                let copiedEvidence = XCTAttachment(screenshot: app.screenshot()); copiedEvidence.name = "Actual large Claude code copied exactly"; copiedEvidence.lifetime = .keepAlways; add(copiedEvidence)
                composer.tap()
                composer.press(forDuration: 1.2)
                let selectAll = app.menuItems["Select All"]
                if selectAll.waitForExistence(timeout: 3) { selectAll.tap(); composer.typeText(XCUIKeyboardKey.delete.rawValue) }
                app.swipeDown()
            }
            for _ in 0..<24 {
                if older.isHittable { break }
                conversation.swipeDown()
            }
            XCTAssertTrue(older.isHittable, "Older pages must remain available beyond the former file-size limit")
            older.tap()
            let previous = app.staticTexts["Synthetic large Claude assistant 079"]
            XCTAssertTrue(previous.waitForExistence(timeout: 20), "An actual older backend page must be rendered")
            XCTAssertEqual(app.staticTexts.matching(NSPredicate(format: "label == %@", "Synthetic large Claude assistant 079")).count, 1)
            let page = XCTAttachment(screenshot: app.screenshot()); page.name = "Synthetic large Claude history older page"; page.lifetime = .keepAlways; add(page)
            app.coordinate(withNormalizedOffset: CGVector(dx: 0.01, dy: 0.4)).press(forDuration: 0.1, thenDragTo: app.coordinate(withNormalizedOffset: CGVector(dx: 0.85, dy: 0.4)))
            XCTAssertTrue(row.waitForExistence(timeout: 10)); row.tap()
            XCTAssertTrue(latest.waitForExistence(timeout: 20))
            XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: NSPredicate(format: "hittable == true"), object: latest)], timeout: 10), .completed)
            XCTAssertTrue(app.staticTexts[config.expectedContext].exists)
            let reopened = XCTAttachment(screenshot: app.screenshot()); reopened.name = "Synthetic large Claude history reopened at latest reply"; reopened.lifetime = .keepAlways; add(reopened)
            return
        }
        if config.mode == "readOnly" {
            guard let response = config.finalResponse else { XCTFail("Read-only original response required"); return }
            XCTAssertTrue(app.staticTexts[response].waitForExistence(timeout: 20))
            XCTAssertFalse(app.buttons["sendReply"].isEnabled, "Dead native owner must not offer a send route")
            app.coordinate(withNormalizedOffset: CGVector(dx: 0.01, dy: 0.4)).press(forDuration: 0.1, thenDragTo: app.coordinate(withNormalizedOffset: CGVector(dx: 0.85, dy: 0.4)))
            XCTAssertTrue(row.waitForExistence(timeout: 10)); row.tap()
            XCTAssertTrue(app.staticTexts[config.expectedContext].waitForExistence(timeout: 20))
            XCTAssertTrue(app.staticTexts[response].waitForExistence(timeout: 20))
            let reopened = XCTAttachment(screenshot: app.screenshot()); reopened.name = "Actual SSH original Claude history after native exit"; reopened.lifetime = .keepAlways; add(reopened)
            return
        }
        if config.mode == "native" {
            guard let reply = config.reply, let response = config.finalResponse else { XCTFail("Native prompt and response required"); return }
            let composer = app.textViews["composer"]
            XCTAssertTrue(composer.waitForExistence(timeout: 10))
            composer.tap(); composer.typeText(reply)
            XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: NSPredicate(format: "enabled == true"), object: app.buttons["sendReply"])], timeout: 20), .completed)
            app.buttons["sendReply"].tap()
            let result = app.staticTexts[response]
            XCTAssertTrue(result.waitForExistence(timeout: 30))
            composer.tap(); composer.typeText("Unsent receipt readiness check")
            XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: NSPredicate(format: "enabled == true"), object: app.buttons["sendReply"])], timeout: 20), .completed, "Verified native reply receipt must restore the composer without replay")
            composer.typeText(String(repeating: XCUIKeyboardKey.delete.rawValue, count: "Unsent receipt readiness check".count))
            XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: NSPredicate(format: "hittable == true"), object: result)], timeout: 10), .completed, "Latest native reply must be visible above the keyboard")
            XCTAssertTrue(app.keyboards.firstMatch.exists)
            let keyboard = XCTAttachment(screenshot: app.screenshot()); keyboard.name = "Actual OpenCode original reply with keyboard"; keyboard.lifetime = .keepAlways; add(keyboard)
            app.coordinate(withNormalizedOffset: CGVector(dx: 0.01, dy: 0.4)).press(forDuration: 0.1, thenDragTo: app.coordinate(withNormalizedOffset: CGVector(dx: 0.85, dy: 0.4)))
            XCTAssertTrue(row.waitForExistence(timeout: 10)); row.tap()
            XCTAssertTrue(result.waitForExistence(timeout: 20))
            XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: NSPredicate(format: "hittable == true"), object: result)], timeout: 10), .completed)
            let reopened = XCTAttachment(screenshot: app.screenshot()); reopened.name = "Actual OpenCode exact conversation reopened latest reply"; reopened.lifetime = .keepAlways; add(reopened)
            return
        }
        let restoredComposer = app.textViews["composer"]
        restoredComposer.tap(); restoredComposer.typeText("saved reconnect draft")
        XCUIDevice.shared.press(.home); app.activate()
        XCTAssertTrue(restoredComposer.waitForExistence(timeout: 15))
        XCTAssertEqual(restoredComposer.value as? String, "saved reconnect draft")
        let sendEnabled = XCTNSPredicateExpectation(predicate: NSPredicate(format: "enabled == true"), object: app.buttons["sendReply"])
        XCTAssertEqual(XCTWaiter.wait(for: [sendEnabled], timeout: 20), .completed, "Foreground reconnect must reattach the exact existing conversation")
        restoredComposer.tap(); restoredComposer.typeText(String(repeating: XCUIKeyboardKey.delete.rawValue, count: "saved reconnect draft".count))
        let proceed = app.buttons["questionOption:probe:Proceed"]
        XCTAssertTrue(proceed.waitForExistence(timeout: 15))
        proceed.tap()
        app.buttons["submitAnswer"].tap()
        let composer = app.textViews["composer"]
        XCTAssertTrue(composer.waitForExistence(timeout: 5))
        composer.tap(); composer.typeText("Exact synthetic mobile reply\nsecond line")
        XCTAssertEqual(composer.value as? String, "Exact synthetic mobile reply\nsecond line")
        let screenshot = XCTAttachment(screenshot: app.screenshot())
        screenshot.name = "Actual SSH conversation with native multiline keyboard"
        screenshot.lifetime = .keepAlways
        add(screenshot)
        app.buttons["sendReply"].tap()
        XCTAssertTrue(app.staticTexts["Accepted by the existing provider conversation."].waitForExistence(timeout: 20))
        XCTAssertEqual(composer.value as? String, "")
        XCTAssertTrue(app.staticTexts["Fixture received the exact multiline reply."].waitForExistence(timeout: 20))
        if let ready = config.adoptReadyPath, let removed = config.adoptRemovedPath {
            try Data("ready".utf8).write(to: URL(fileURLWithPath: ready), options: .atomic)
            let removedPredicate = NSPredicate { _, _ in FileManager.default.fileExists(atPath: removed) }
            XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: removedPredicate, object: nil)], timeout: 30), .completed)
            XCTAssertEqual(try String(contentsOfFile: removed, encoding: .utf8).trimmingCharacters(in: .whitespacesAndNewlines), config.threadId)
            app.coordinate(withNormalizedOffset: CGVector(dx: 0.01, dy: 0.4))
                .press(forDuration: 0.1, thenDragTo: app.coordinate(withNormalizedOffset: CGVector(dx: 0.85, dy: 0.4)))
            app.buttons["addThread"].tap(); app.buttons["Add an existing thread"].tap()
            let candidate = app.buttons["candidate-" + config.threadId]
            XCTAssertTrue(candidate.waitForExistence(timeout: 20))
            candidate.tap()
            XCTAssertTrue(app.staticTexts[config.expectedContext].waitForExistence(timeout: 20))
            XCTAssertTrue(app.staticTexts["Fixture received the exact multiline reply."].waitForExistence(timeout: 20))
        }
        // Independent fixture-owner observations remain mandatory: UI receipt
        // alone cannot prove original provider continuation/no replacement.
    }
}
