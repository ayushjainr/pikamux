import Foundation
import XCTest

/// Descriptor import is DEBUG-only. This proves the real claim/Keychain/SSH path,
/// not physical camera scanning. Only explicitly supplied disposable endpoints.
final class PikaQRIntegrationTests: XCTestCase {
    struct Configuration: Decodable { let qr: String; let nodeId: String; let storeId: String?; let expectedName: String?; let expectedFailure: String?; let pendingExpected: Bool? }
    @MainActor func testDisposablePairingFailure() throws {
        continueAfterFailure = false
        guard let path = ProcessInfo.processInfo.environment["PIKA_PAIR_TEST_CONFIG"], !path.isEmpty, !path.contains("$(") else { throw XCTSkip("No disposable failure endpoint configured.") }
        let config = try JSONDecoder().decode(Configuration.self, from: Data(contentsOf: URL(fileURLWithPath: path)))
        guard let expected = config.expectedFailure else { throw XCTSkip("Success configuration, not a failure journey.") }
        let app = XCUIApplication(); app.launchArguments = ["--ssh-integration-test"]
        app.launchEnvironment = ["PIKA_UI_TEST_PAIRING_QR": config.qr, "PIKA_UI_TEST_STORE_ID": config.storeId ?? UUID().uuidString]
        app.launch(); app.buttons["Add a machine"].tap(); app.buttons["importPairingCode"].tap()
        XCTAssertTrue(app.staticTexts[expected].waitForExistence(timeout: 30))
        XCTAssertFalse(app.buttons["I verified this fingerprint"].exists)
        XCTAssertEqual(app.buttons["finishPairing"].exists, config.pendingExpected ?? false)
        let proof = XCTAttachment(screenshot: app.screenshot()); proof.name = "Actual disposable pairing failure"; proof.lifetime = .keepAlways; add(proof)
        if config.pendingExpected == true {
            app.buttons["Discard unfinished pairing"].tap(); app.buttons["Discard"].tap()
            XCTAssertFalse(app.buttons["finishPairing"].exists)
        }
        app.buttons["Cancel"].tap(); XCTAssertTrue(app.staticTexts["No saved board"].exists)
    }
    @MainActor func testDisposableQRClaimAndSavedSSH() throws {
        continueAfterFailure = false
        guard let path = ProcessInfo.processInfo.environment["PIKA_PAIR_TEST_CONFIG"], !path.isEmpty, !path.contains("$(") else {
            throw XCTSkip("No explicitly scoped disposable pairing endpoint configured.")
        }
        let config = try JSONDecoder().decode(Configuration.self, from: Data(contentsOf: URL(fileURLWithPath: path)))
        let app = XCUIApplication()
        app.launchArguments = ["--ssh-integration-test"]
        app.launchEnvironment = ["PIKA_UI_TEST_PAIRING_QR": config.qr, "PIKA_UI_TEST_STORE_ID": config.storeId ?? UUID().uuidString]
        app.launch()
        app.buttons["Add a machine"].tap()
        XCTAssertTrue(app.buttons["scanPairing"].waitForExistence(timeout: 5))
        app.buttons["importPairingCode"].tap()
        let connected = NSPredicate(format: "label BEGINSWITH %@ OR label BEGINSWITH %@", "Observed", "Connected")
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: connected, object: app.staticTexts["connectionStatus"])], timeout: 40), .completed)
        if let name = config.expectedName { XCTAssertTrue(app.staticTexts[name].waitForExistence(timeout: 5)) }
        XCTAssertFalse(app.buttons["I verified this fingerprint"].exists, "QR pin must be checked, not replaced by human TOFU.")
        app.buttons["Machine connections"].tap()
        app.buttons["Details"].tap()
        XCTAssertTrue(app.staticTexts["Verified Pika identity: " + config.nodeId].exists)
        app.buttons["Cancel"].tap()
        let paired = XCTAttachment(screenshot: app.screenshot()); paired.name = "Actual disposable QR paired board"; paired.lifetime = .keepAlways; add(paired)
        app.terminate(); app.launch()
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: connected, object: app.staticTexts["connectionStatus"])], timeout: 30), .completed)
        if let name = config.expectedName { XCTAssertTrue(app.staticTexts[name].waitForExistence(timeout: 5)) }
        XCTAssertFalse(app.buttons["I verified this fingerprint"].exists)
    }
    @MainActor func testInvalidQRKeepsManualFallback() {
        let app = XCUIApplication()
        app.launchArguments = ["--ssh-integration-test"]
        app.launchEnvironment = ["PIKA_UI_TEST_PAIRING_QR": "pika://pair?data=invalid", "PIKA_UI_TEST_STORE_ID": UUID().uuidString]
        app.launch(); app.buttons["Add a machine"].tap()
        let primary = XCTAttachment(screenshot: app.screenshot()); primary.name = "QR primary with manual login collapsed"; primary.lifetime = .keepAlways; add(primary)
        app.buttons["importPairingCode"].tap()
        XCTAssertTrue(app.buttons["scanPairing"].waitForExistence(timeout: 5))
        XCTAssertFalse(app.buttons["finishPairing"].exists, "Malformed code must not generate a key.")
        app.buttons["Manual login"].tap()
        XCTAssertTrue(app.textFields["machineAddress"].exists)
        app.buttons["Cancel"].tap()
        XCTAssertTrue(app.staticTexts["No saved board"].exists)
    }
}
