import XCTest

final class PikaComposerTests: XCTestCase {
    @MainActor private func open(_ arguments: [String] = []) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = ["--ui-fixture"] + arguments
        app.launch()
        XCTAssertTrue(app.buttons["thread-fixture-one"].waitForExistence(timeout: 5))
        app.buttons["thread-fixture-one"].tap()
        XCTAssertTrue(app.textViews["composer"].waitForExistence(timeout: 5))
        return app
    }
    @MainActor func testModelIsCheckedExactThreadSettingNotLiteralMessage() {
        let app = open()
        app.buttons["composerCommands"].tap()
        app.buttons["commandModel"].tap()
        XCTAssertTrue(app.staticTexts["currentThreadModel"].label.contains("fixture-current"))
        app.buttons["modelChoice:fixture-alternative"].tap()
        XCTAssertTrue(app.buttons["composerCommands"].waitForExistence(timeout: 5))
        XCTAssertEqual(app.textViews["composer"].value as? String, "")
        app.buttons["composerCommands"].tap()
        app.buttons["commandModel"].tap()
        XCTAssertTrue(app.staticTexts["currentThreadModel"].label.contains("fixture-alternative"))
    }
    @MainActor func testTypedSlashModelAndDollarOpenNativePickers() {
        let app = open()
        let composer = app.textViews["composer"]
        composer.tap()
        composer.typeText("/model")
        if app.buttons["commandModel"].waitForExistence(timeout: 3) { app.buttons["commandModel"].tap() }
        else { app.buttons["sendReply"].tap() }
        XCTAssertTrue(app.buttons["modelChoice:fixture-current"].waitForExistence(timeout: 5))
        app.buttons["modelChoice:fixture-current"].tap()
        XCTAssertTrue(composer.waitForExistence(timeout: 5))
        XCTAssertEqual(composer.value as? String, "")
        composer.tap()
        composer.typeText("$")
        XCTAssertTrue(app.buttons["skillChoice:fixture-review"].waitForExistence(timeout: 5))
        app.buttons["skillChoice:fixture-review"].tap()
        XCTAssertEqual(composer.value as? String, "$fixture-review ")
    }
    @MainActor func testSkillSearchInsertsReferenceWithoutReplacingMessage() {
        let app = open()
        app.textViews["composer"].tap()
        app.textViews["composer"].typeText("Please inspect $HOME")
        app.buttons["composerSkills"].tap()
        XCTAssertTrue(app.buttons["skillChoice:fixture-review"].waitForExistence(timeout: 5))
        app.searchFields.firstMatch.tap()
        app.searchFields.firstMatch.typeText("review")
        app.buttons["skillChoice:fixture-review"].tap()
        XCTAssertEqual(app.textViews["composer"].value as? String, "Please inspect $HOME $fixture-review ")
    }
    @MainActor func testOrdinarySlashPathIsSentAsLiteralMessage() {
        let app = open()
        let composer = app.textViews["composer"]
        composer.tap()
        composer.typeText("/mnt/project contains $HOME")
        // A first-character slash may open the picker; dismiss it without consuming the draft.
        if app.buttons["Close"].exists { app.buttons["Close"].tap() }
        app.buttons["sendReply"].tap()
        XCTAssertTrue(app.staticTexts["/mnt/project contains $HOME"].waitForExistence(timeout: 5))
        XCTAssertEqual(composer.value as? String, "")
    }
    @MainActor func testUnsupportedProviderAndOfflineNeverOfferWorkingControl() {
        let app = open(["--fixture-controls-unsupported"])
        app.buttons["composerCommands"].tap()
        XCTAssertTrue(app.staticTexts["Provider controls are unavailable. Nothing was sent."].waitForExistence(timeout: 5))
        XCTAssertFalse(app.buttons["commandModel"].exists)
        app.buttons["Close"].tap()
        app.buttons["fixtureOffline"].tap()
        XCTAssertFalse(app.buttons["composerCommands"].isEnabled)
        XCTAssertFalse(app.buttons["composerSkills"].isEnabled)
    }
    @MainActor func testUnknownModelUpdateDoesNotClaimSettingOrEnableAnotherChange() {
        let app = open(["--fixture-model-unknown"])
        app.buttons["composerCommands"].tap()
        app.buttons["commandModel"].tap()
        app.buttons["modelChoice:fixture-alternative"].tap()
        XCTAssertTrue(app.staticTexts["The change was not confirmed. Reopen controls to read the actual setting before another change."].waitForExistence(timeout: 5))
        XCTAssertFalse(app.buttons["modelChoice:fixture-current"].isEnabled)
        XCTAssertTrue(app.staticTexts["currentThreadModel"].label.contains("fixture-current"))
    }
}
