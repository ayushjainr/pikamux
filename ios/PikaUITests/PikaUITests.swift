import XCTest

final class PikaUITests: XCTestCase {
    @MainActor private func swipeBack() {
        app.coordinate(withNormalizedOffset: CGVector(dx: 0.01, dy: 0.4))
            .press(forDuration: 0.1, thenDragTo: app.coordinate(withNormalizedOffset: CGVector(dx: 0.85, dy: 0.4)))
    }

    @MainActor func testDexToCompactThreadKeyboardJourney() {
        launchFixture()
        XCTAssertTrue(app.buttons["dexTab"].isHittable)
        XCTAssertTrue(app.buttons["pikaTab"].isHittable)
        XCTAssertEqual(app.buttons["dexTab"].frame.midY, app.buttons["pikaTab"].frame.midY, accuracy: 1)
        XCTAssertFalse(app.tabBars.firstMatch.exists, "The custom Dex navigation must not leave an empty system tab bar")
        let board = XCTAttachment(screenshot: app.screenshot())
        board.name = "Pokédex native board"; board.lifetime = .keepAlways; add(board)
        app.buttons["addThread"].tap()
        XCTAssertTrue(app.buttons["Start a thread"].waitForExistence(timeout: 3))
        app.buttons["Start a thread"].tap()
        XCTAssertTrue(app.buttons["Cancel"].waitForExistence(timeout: 3))
        app.buttons["Cancel"].tap()
        app.buttons["thread-fixture-one"].tap()
        let composer = app.textViews["composer"]
        XCTAssertTrue(composer.waitForExistence(timeout: 5))
        XCTAssertFalse(app.buttons["addThread"].exists)
        XCTAssertFalse(app.buttons["dexTab"].exists)
        XCTAssertFalse(app.navigationBars.firstMatch.exists)
        // Casing includes the status safe area; the content remains a single compact row.
        XCTAssertLessThan(app.otherElements["threadCasingHeader"].frame.height, app.frame.height * 0.14)
        XCTAssertEqual(app.otherElements["threadSignal"].value as? String, "Needs you")
        XCTAssertLessThan(app.otherElements["threadSignal"].frame.maxX, app.staticTexts["conversationHeader"].frame.minX)
        XCTAssertGreaterThan(composer.frame.minY, app.frame.height * 0.75, "Composer must dock at the bottom even for short history")
        let thread = XCTAttachment(screenshot: app.screenshot())
        thread.name = "Compact native thread keyboard closed"; thread.lifetime = .keepAlways; add(thread)
        composer.tap(); composer.typeText("first line\nsecond line")
        XCTAssertTrue(app.keyboards.firstMatch.waitForExistence(timeout: 3))
        XCTAssertEqual(composer.value as? String, "first line\nsecond line")
        let layout = XCTAttachment(string: app.debugDescription)
        layout.name = "Native keyboard layout tree"; layout.lifetime = .keepAlways; add(layout)
        let keyboard = XCTAttachment(screenshot: app.screenshot())
        keyboard.name = "Compact native thread keyboard open"; keyboard.lifetime = .keepAlways; add(keyboard)
        // iOS exposes the 44pt prediction strip separately from Keyboard.
        let predictions = app.otherElements["Typing Predictions"]
        let keyboardTop = predictions.exists ? min(predictions.frame.minY, app.keyboards.firstMatch.frame.minY) : app.keyboards.firstMatch.frame.minY
        XCTAssertLessThanOrEqual(composer.frame.maxY, keyboardTop + 1)
        XCTAssertLessThan(keyboardTop - composer.frame.maxY, 33)
        XCTAssertTrue(app.buttons["sendReply"].isHittable)
        swipeBack()
        XCTAssertTrue(app.buttons["dexTab"].waitForExistence(timeout: 3))
        app.buttons["thread-fixture-two"].tap()
        XCTAssertEqual(composer.value as? String, "")
        swipeBack()
        app.buttons["thread-fixture-one"].tap()
        XCTAssertEqual(composer.value as? String, "first line\nsecond line")
        app.buttons["sendReply"].tap()
        XCTAssertTrue(app.staticTexts["first line\nsecond line"].waitForExistence(timeout: 5))
        XCTAssertEqual(composer.value as? String, "")
        swipeBack()
        app.buttons["pikaTab"].tap()
        XCTAssertTrue(app.buttons["Open Pika"].waitForExistence(timeout: 3))
        app.buttons["dexTab"].tap()
        XCTAssertTrue(app.buttons["thread-fixture-one"].waitForExistence(timeout: 3))
    }
    @MainActor func testRichReplyReadingAndCodeCopy() {
        launchFixture()
        app.terminate(); app.launchArguments = ["--ui-fixture", "--fixture-markdown"]; app.launch()
        app.buttons["thread-fixture-one"].tap()
        let copy = app.buttons["Copy code"]
        XCTAssertTrue(copy.waitForExistence(timeout: 5))
        if !copy.isHittable { app.swipeUp() }
        let rich = XCTAttachment(screenshot: app.screenshot()); rich.name = "Rich reply code and table"; rich.lifetime = .keepAlways; add(rich)
        copy.tap()
        XCTAssertTrue(app.buttons["Code copied"].exists)
        let composer = app.textViews["composer"]
        composer.press(forDuration: 1.2)
        let paste = app.menuItems["Paste"]
        if paste.waitForExistence(timeout: 3) { paste.tap() }
        else { app.buttons["Paste"].tap() }
        XCTAssertEqual(composer.value as? String, "printf 'hello from Pika\\n'\necho 'ready'")
        XCTAssertTrue(app.buttons["sendReply"].isEnabled)
        app.swipeDown(); app.swipeDown()
        XCTAssertTrue(app.staticTexts["Ready for your review"].exists)
        let heading = XCTAttachment(screenshot: app.screenshot()); heading.name = "Rich reply headings and lists"; heading.lifetime = .keepAlways; add(heading)
    }
    @MainActor func testNormalColoredTailTabSelection() {
        let normal = XCUIApplication()
        normal.launch()
        XCTAssertTrue(normal.staticTexts["No saved board"].waitForExistence(timeout: 5))
        let before = XCTAttachment(screenshot: normal.screenshot()); before.name = "Colored inactive tail"; before.lifetime = .keepAlways; add(before)
        normal.buttons["pikaTab"].tap()
        XCTAssertTrue(normal.staticTexts["Your existing Pika"].waitForExistence(timeout: 5))
        let after = XCTAttachment(screenshot: normal.screenshot()); after.name = "Colored selected tail"; after.lifetime = .keepAlways; add(after)
        normal.buttons["dexTab"].tap()
        XCTAssertTrue(normal.staticTexts["No saved board"].exists)
    }
    @MainActor private var app: XCUIApplication!
    @MainActor private func launchFixture() {
        continueAfterFailure = false
        app = XCUIApplication()
        app.launchArguments = ["--ui-fixture"]
        app.launch()
        XCTAssertTrue(app.staticTexts["fixtureBanner"].waitForExistence(timeout: 5))
    }
    @MainActor func testNativeReturnDraftIsolationAndExplicitSend() {
        launchFixture()
        let boardImage = XCTAttachment(screenshot: app.screenshot()); boardImage.name = "Native branded board visual"; boardImage.lifetime = .keepAlways; add(boardImage)
        app.buttons["thread-fixture-one"].tap()
        let composer = app.textViews["composer"]
        XCTAssertTrue(composer.waitForExistence(timeout: 5))
        composer.tap(); composer.typeText("first line\nsecond line")
        XCTAssertEqual(composer.value as? String, "first line\nsecond line")
        XCTAssertTrue(app.buttons["sendReply"].isEnabled)
        let keyboardImage = XCTAttachment(screenshot: app.screenshot()); keyboardImage.name = "Native multiline keyboard visual"; keyboardImage.lifetime = .keepAlways; add(keyboardImage)
        XCTAssertFalse(app.staticTexts["Accepted by the existing provider conversation."].exists)
        swipeBack()
        app.buttons["thread-fixture-two"].tap()
        XCTAssertEqual(composer.value as? String, "")
        composer.tap(); composer.typeText("beta draft")
        swipeBack()
        app.buttons["thread-fixture-one"].tap()
        XCTAssertEqual(composer.value as? String, "first line\nsecond line")
        app.buttons["sendReply"].tap()
        XCTAssertTrue(app.staticTexts["first line\nsecond line"].waitForExistence(timeout: 5))
        XCTAssertEqual(composer.value as? String, "")
        let conversationImage = XCTAttachment(screenshot: app.screenshot()); conversationImage.name = "Native conversation after send"; conversationImage.lifetime = .keepAlways; add(conversationImage)
    }
    @MainActor func testOfflineKeepsNativeDraftAndDisablesSend() {
        launchFixture()
        app.buttons["thread-fixture-one"].tap()
        let composer = app.textViews["composer"]
        XCTAssertTrue(composer.waitForExistence(timeout: 5))
        composer.tap(); composer.typeText("keep this\nwhile offline")
        app.buttons["fixtureOffline"].tap()
        XCTAssertEqual(composer.value as? String, "keep this\nwhile offline")
        XCTAssertFalse(app.buttons["sendReply"].isEnabled)
    }
    @MainActor func testUnknownReceiptNeverAutomaticallyRepeats() {
        launchFixture()
        app.terminate()
        app.launchArguments = ["--ui-fixture", "--fixture-unknown-outcome"]
        app.launch()
        app.buttons["thread-fixture-one"].tap()
        let composer = app.textViews["composer"]
        XCTAssertTrue(composer.waitForExistence(timeout: 5))
        composer.tap(); composer.typeText("one uncertain message")
        app.buttons["sendReply"].tap()
        XCTAssertTrue(app.staticTexts["Outcome unknown. Your text is saved; it was not repeated."].waitForExistence(timeout: 5))
        XCTAssertEqual(composer.value as? String, "one uncertain message")
        XCTAssertFalse(app.buttons["sendReply"].isEnabled)
        XCTAssertEqual(app.staticTexts["fixtureSendCount"].label, "Fixture send attempts: 1")
    }
    @MainActor func testManualAddressEntryAndCancellation() {
        launchFixture()
        app.terminate(); app.launchArguments = ["--ui-fixture", "--fixture-live-header"]; app.launch()
        app.buttons["Machine connections"].tap()
        app.buttons["Manual login"].tap()
        let address = app.textFields["machineAddress"]
        XCTAssertTrue(address.waitForExistence(timeout: 5))
        address.tap(); address.typeText("fixture.invalid")
        for _ in 0..<3 where !app.buttons["connectMachine"].exists { app.swipeUp() }
        XCTAssertFalse(app.buttons["connectMachine"].isEnabled)
        app.buttons["Cancel"].tap()
        XCTAssertTrue(app.buttons["thread-fixture-one"].exists)
        app.buttons["thread-fixture-one"].tap()
        let composer = app.textViews["composer"]
        XCTAssertTrue(composer.waitForExistence(timeout: 5))
        composer.tap(); composer.typeText("feed still connected after settings cancel")
        app.buttons["sendReply"].tap()
        XCTAssertTrue(app.staticTexts["Fixture send attempts: 1"].waitForExistence(timeout: 5), "Canceling settings must not fence out the existing event feed")
        XCTAssertEqual(composer.label, "Reply to Updated original thread on Fixture Alpha")
        XCTAssertEqual(app.buttons["sendReply"].label, "Send reply to Updated original thread on Fixture Alpha")
        XCTAssertTrue(app.staticTexts["conversationHeader"].label.contains("Updated original thread"))
        XCTAssertTrue(app.staticTexts["conversationHeader"].label.contains("Ready"))
        XCTAssertEqual(app.otherElements["threadSignal"].value as? String, "Ready")
        app.buttons["fixtureOffline"].tap()
        XCTAssertEqual(app.otherElements["threadSignal"].value as? String, "Inactive")
    }
    @MainActor func testPaginatedReadOnlyReceiptDoesNotRepeatSend() {
        launchFixture()
        app.terminate(); app.launchArguments = ["--ui-fixture", "--fixture-unknown-outcome"]; app.launch()
        app.buttons["thread-fixture-one"].tap()
        let composer = app.textViews["composer"]
        XCTAssertTrue(composer.waitForExistence(timeout: 5))
        composer.tap(); composer.typeText("receipt fixture text")
        app.buttons["sendReply"].tap()
        let receipt = app.buttons["Check original delivery receipt"]
        XCTAssertTrue(receipt.waitForExistence(timeout: 5)); receipt.tap()
        XCTAssertTrue(app.staticTexts["More original history remains. Check again to continue the read-only receipt search."].waitForExistence(timeout: 5))
        receipt.tap()
        XCTAssertTrue(app.staticTexts["The original machine confirmed this message was accepted. It was not sent again."].waitForExistence(timeout: 5))
        XCTAssertEqual(composer.value as? String, "")
        XCTAssertEqual(app.staticTexts["fixtureSendCount"].label, "Fixture send attempts: 1")
    }
    @MainActor func testNativeOlderHistoryAndOneTimeCommandApproval() {
        launchFixture()
        app.terminate(); app.launchArguments = ["--ui-fixture", "--fixture-approval"]; app.launch()
        app.buttons["thread-fixture-one"].tap()
        let accept = app.buttons["acceptProviderRequest"]
        XCTAssertTrue(accept.waitForExistence(timeout: 5)); XCTAssertTrue(accept.isEnabled)
        XCTAssertTrue(app.staticTexts["printf 'disposable fixture'"].exists)
        accept.tap()
        XCTAssertFalse(accept.isEnabled)
        app.swipeDown()
        app.buttons["Load older context"].tap()
        XCTAssertTrue(app.staticTexts["Older exact fixture history"].waitForExistence(timeout: 5))
    }
    @MainActor func testNativeLateFileApprovalRetainsOriginalDiff() {
        launchFixture()
        app.terminate(); app.launchArguments = ["--ui-fixture", "--fixture-approval"]; app.launch()
        app.buttons["thread-fixture-two"].tap()
        XCTAssertTrue(app.staticTexts["/fixture/notes.txt"].waitForExistence(timeout: 5))
        XCTAssertTrue(app.staticTexts["-old\n+reviewed fixture"].exists)
        XCTAssertTrue(app.buttons["acceptProviderRequest"].isEnabled)
    }
    @MainActor func testObservedApprovalResolutionBeforeResponseDoesNotBlockFutureText() {
        launchFixture()
        app.terminate(); app.launchArguments = ["--ui-fixture", "--fixture-approval", "--fixture-fast-resolution"]; app.launch()
        app.buttons["thread-fixture-one"].tap()
        let accept = app.buttons["acceptProviderRequest"]
        XCTAssertTrue(accept.waitForExistence(timeout: 5)); accept.tap()
        XCTAssertTrue(app.staticTexts["The original request closed. This does not confirm the decision was accepted."].waitForExistence(timeout: 5))
        XCTAssertFalse(app.staticTexts["Decision submitted. Waiting for the original request to close."].exists)
        let composer = app.textViews["composer"]
        composer.tap(); composer.typeText("after exact resolution")
        XCTAssertTrue(app.buttons["sendReply"].isEnabled)
        app.buttons["sendReply"].tap()
        XCTAssertTrue(app.staticTexts["Accepted by the UI fixture only."].waitForExistence(timeout: 5))
    }
    @MainActor func testObservedResolutionWinsLateUnknownRequestStatus() {
        launchFixture(); app.terminate(); app.launchArguments = ["--ui-fixture", "--fixture-approval", "--fixture-status-race"]; app.launch()
        app.buttons["thread-fixture-one"].tap()
        let accept = app.buttons["acceptProviderRequest"]
        XCTAssertTrue(accept.waitForExistence(timeout: 5)); accept.tap()
        app.buttons["Check original request status"].tap()
        XCTAssertTrue(app.staticTexts["The original request closed. This does not confirm the decision was accepted."].waitForExistence(timeout: 5))
        XCTAssertFalse(app.staticTexts["The decision remains pending or uncertain. It will not be repeated and does not block unrelated replies."].exists)
        let composer = app.textViews["composer"]; composer.tap(); composer.typeText("after observed close")
        app.buttons["sendReply"].tap()
        XCTAssertTrue(app.staticTexts["Accepted by the UI fixture only."].waitForExistence(timeout: 5))
    }
    @MainActor func testNativeMachineProjectPagingAndReadOnlyCreationReceipt() {
        launchFixture()
        app.terminate(); app.launchArguments = ["--ui-fixture", "--fixture-mutations"]; app.launch()
        app.buttons["addThread"].tap(); app.buttons["Start a thread"].tap()
        let name = app.textFields["newThreadName"]
        XCTAssertTrue(name.waitForExistence(timeout: 5)); name.tap(); name.typeText("Disposable native creation")
        app.buttons["threadMachinePicker"].tap()
        app.buttons["Fixture Beta"].tap()
        app.buttons["threadProjectPicker"].tap()
        let second = app.buttons["Second page project"]
        XCTAssertTrue(second.waitForExistence(timeout: 5)); second.tap()
        let start = app.buttons["startThread"]
        XCTAssertTrue(start.isEnabled); start.tap()
        XCTAssertTrue(app.staticTexts["Creation not confirmed. It will not be repeated automatically."].waitForExistence(timeout: 5))
        XCTAssertFalse(start.isEnabled)
        app.buttons["Check original creation receipts"].tap()
        XCTAssertTrue(app.staticTexts["The original creation is confirmed. No replacement was launched."].waitForExistence(timeout: 5))
        XCTAssertTrue(start.isEnabled)
    }
    @MainActor func testAssistantLateApprovalHistoryAndReturnToExactProject() {
        launchFixture()
        app.terminate(); app.launchArguments = ["--ui-fixture", "--fixture-approval"]; app.launch()
        app.buttons["pikaTab"].tap(); app.buttons["Open Pika"].tap()
        XCTAssertTrue(app.staticTexts["/fixture/notes.txt"].waitForExistence(timeout: 5))
        XCTAssertTrue(app.buttons["acceptProviderRequest"].isEnabled)
        app.buttons["Load older context"].tap()
        XCTAssertTrue(app.staticTexts["Older exact fixture history"].waitForExistence(timeout: 5))
        swipeBack()
        app.buttons["dexTab"].tap(); app.buttons["thread-fixture-one"].tap()
        XCTAssertTrue(app.staticTexts["This is disposable UI fixture context for Fixture Alpha. No real provider is attached."].waitForExistence(timeout: 5))
        XCTAssertFalse(app.staticTexts["/fixture/notes.txt"].exists)
    }
    @MainActor func testNativeRecentHistoryFollowAndReadingAnchor() {
        launchFixture(); app.terminate(); app.launchArguments = ["--ui-fixture", "--fixture-long-history"]; app.launch()
        app.buttons["thread-fixture-one"].tap()
        let latest = app.staticTexts.matching(NSPredicate(format: "label BEGINSWITH %@", "Original fixture context 29")).firstMatch
        XCTAssertTrue(latest.waitForExistence(timeout: 5))
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: NSPredicate(format: "hittable == true"), object: latest)], timeout: 5), .completed, "Initial read must land on recent context")
        let composer = app.textViews["composer"]
        composer.tap(); composer.typeText("follow while at bottom"); app.buttons["sendReply"].tap()
        XCTAssertTrue(app.staticTexts["Live fixture output"].waitForExistence(timeout: 5))
        XCTAssertTrue(app.staticTexts["Live fixture output"].isHittable)
        for _ in 0..<25 {
            if app.buttons["Load older context"].isHittable { break }
            app.scrollViews["conversation-fixture-one"].swipeDown()
        }
        XCTAssertTrue(app.buttons["Load older context"].isHittable)
        app.buttons["Load older context"].tap()
        let original = app.staticTexts["This is disposable UI fixture context for Fixture Alpha. No real provider is attached."]
        XCTAssertTrue(original.waitForExistence(timeout: 5)); XCTAssertTrue(original.isHittable)
        composer.tap(); composer.typeText("resume following after reading older context"); app.buttons["sendReply"].tap()
        XCTAssertTrue(app.staticTexts["Accepted by the UI fixture only."].waitForExistence(timeout: 5))
        let reply = app.staticTexts.matching(NSPredicate(format: "label == %@", "Live fixture output")).allElementsBoundByIndex.last!
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: NSPredicate(format: "hittable == true"), object: reply)], timeout: 5), .completed, "Explicit Send must leave older history and show the reply above the keyboard")
        XCTAssertFalse(original.isHittable)
        let evidence = XCTAttachment(screenshot: app.screenshot()); evidence.name = "Send from older history follows reply"; evidence.lifetime = .keepAlways; add(evidence)
        swipeBack()
        app.buttons["pikaTab"].tap(); app.buttons["Open Pika"].tap()
        XCTAssertTrue(latest.waitForExistence(timeout: 5))
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: NSPredicate(format: "hittable == true"), object: latest)], timeout: 5), .completed, "Preloaded assistant history must also open at recent context")
    }
    @MainActor func testStreamingReplyFollowsUntilUserReadsOlderMessages() {
        launchFixture(); app.terminate()
        app.launchArguments = ["--ui-fixture", "--fixture-long-history", "--fixture-streaming-reply"]; app.launch()
        app.buttons["thread-fixture-one"].tap()
        let composer = app.textViews["composer"]
        XCTAssertTrue(composer.waitForExistence(timeout: 5))
        composer.tap(); composer.typeText("follow the whole reply"); app.buttons["sendReply"].tap()
        let tail = app.staticTexts.matching(NSPredicate(format: "label BEGINSWITH %@", "Reply 1 paragraph 12.")).firstMatch
        XCTAssertTrue(tail.waitForExistence(timeout: 20))
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: NSPredicate(format: "hittable == true"), object: tail)], timeout: 5), .completed)
        XCTAssertLessThanOrEqual(tail.frame.maxY, composer.frame.minY, "Reply tail must be visible above the composer, not behind the keyboard")
        let following = XCTAttachment(screenshot: app.screenshot()); following.name = "Long streamed reply follows above keyboard"; following.lifetime = .keepAlways; add(following)
        composer.tap(); composer.typeText("now let me read older messages"); app.buttons["sendReply"].tap()
        let scroll = app.scrollViews["conversation-fixture-one"]
        scroll.swipeDown(); scroll.swipeDown()
        let candidates = app.staticTexts.matching(NSPredicate(format: "label BEGINSWITH %@ OR label BEGINSWITH %@", "Reply 1 paragraph", "Original fixture context"))
        guard let older = candidates.allElementsBoundByIndex.first(where: { $0.isHittable }) else {
            XCTFail("Scrolling up must reveal an older message"); return
        }
        let readingY = older.frame.minY
        let nextTail = app.staticTexts.matching(NSPredicate(format: "label BEGINSWITH %@", "Reply 2 paragraph 12.")).firstMatch
        XCTAssertTrue(nextTail.waitForExistence(timeout: 20))
        XCTAssertFalse(nextTail.isHittable, "Passive streaming must not pull a reader back down")
        XCTAssertEqual(older.frame.minY, readingY, accuracy: 5)
        let reading = XCTAttachment(screenshot: app.screenshot()); reading.name = "Reading anchor retained during later streaming"; reading.lifetime = .keepAlways; add(reading)
    }
    @MainActor func testNativeKeyboardRotationPreservesDraftAndSend() {
        defer { XCUIDevice.shared.orientation = .portrait }
        launchFixture(); app.buttons["thread-fixture-one"].tap()
        let composer = app.textViews["composer"]
        XCTAssertTrue(composer.waitForExistence(timeout: 5))
        composer.tap(); composer.typeText("rotation draft\nsecond line")
        let portrait = XCTAttachment(screenshot: app.screenshot()); portrait.name = "Native portrait keyboard"; portrait.lifetime = .keepAlways; add(portrait)
        XCUIDevice.shared.orientation = .landscapeLeft
        let landscapeReady = NSPredicate { _, _ in self.app.frame.width > self.app.frame.height && self.app.buttons["sendReply"].isHittable }
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: landscapeReady, object: nil)], timeout: 8), .completed)
        XCTAssertEqual(composer.value as? String, "rotation draft\nsecond line")
        composer.tap(); composer.typeText(" at caret")
        XCTAssertEqual(composer.value as? String, "rotation draft\nsecond line at caret")
        XCTAssertTrue(app.buttons["sendReply"].isEnabled)
        let landscape = XCTAttachment(screenshot: XCUIScreen.main.screenshot()); landscape.name = "Native landscape keyboard and send"; landscape.lifetime = .keepAlways; add(landscape)
    }
    @MainActor func testSameAssistantReopenRetainsOriginalPendingRequest() {
        launchFixture()
        app.terminate(); app.launchArguments = ["--ui-fixture", "--fixture-approval"]; app.launch()
        app.buttons["pikaTab"].tap(); app.buttons["Open Pika"].tap()
        XCTAssertTrue(app.staticTexts["/fixture/notes.txt"].waitForExistence(timeout: 5))
        swipeBack()
        app.buttons["Open Pika"].tap()
        let count = app.staticTexts["fixtureAssistantOpenCount"]
        let twice = NSPredicate(format: "label == %@", "Fixture assistant opens: 2")
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: twice, object: count)], timeout: 8), .completed)
        XCTAssertTrue(app.staticTexts["/fixture/notes.txt"].waitForExistence(timeout: 5))
        XCTAssertTrue(app.buttons["acceptProviderRequest"].isEnabled)
    }
    @MainActor func testDelayedAssistantCannotOverwriteNewerProjectSelection() {
        launchFixture()
        app.terminate(); app.launchArguments = ["--ui-fixture", "--fixture-slow-assistant"]; app.launch()
        app.buttons["pikaTab"].tap(); app.buttons["Open Pika"].tap()
        app.buttons["dexTab"].tap(); app.buttons["thread-fixture-one"].tap()
        let completed = app.staticTexts["fixtureAssistantFinished"]
        XCTAssertTrue(completed.waitForExistence(timeout: 5))
        let predicate = NSPredicate(format: "label == %@", "Fixture assistant completed: yes")
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: predicate, object: completed)], timeout: 8), .completed)
        XCTAssertTrue(app.staticTexts["This is disposable UI fixture context for Fixture Alpha. No real provider is attached."].exists)
        XCTAssertFalse(app.staticTexts["This is disposable UI fixture context for Fixture Assistant. No real provider is attached."].exists)
    }
}
