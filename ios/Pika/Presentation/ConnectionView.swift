import SwiftUI
import UniformTypeIdentifiers

struct ConnectionView: View {
    @EnvironmentObject private var model: AppModel
    @Environment(\.dismiss) private var dismiss
    @State private var address = ""
    @State private var username = ""
    @State private var port = "22"
    @State private var password = ""
    @State private var passphrase = ""
    @State private var keyBytes: Data?
    @State private var keyName: String?
    @State private var importKey = false
    @State private var keyMode = false
    @State private var importError: String?
    @State private var scan = false
    @State private var scannedCode: String?
    @State private var manual = false
    @State private var discardPairing = false
    @Environment(\.scenePhase) private var scenePhase
    var body: some View {
        NavigationStack {
            Form {
                Section {
                    Text("1. On your machine, choose Connect phone in Pika.\n2. Scan its code here with Tailscale on both devices.")
                        .font(.callout).foregroundStyle(.secondary)
                    if model.connecting {
                        ProgressView(model.notice ?? "Connecting to your machine…")
                            .accessibilityIdentifier("pairingProgress")
                    } else if model.notice != nil {
                        NoticeCard()
                    }
                    Button("Scan pairing code") { scan = true }.buttonStyle(PikaPrimaryButtonStyle())
                        .disabled(model.connecting).accessibilityIdentifier("scanPairing")
                    if model.pendingPairingAvailable {
                        Button("Finish previous pairing") { model.finishPairing() }
                            .disabled(model.connecting).accessibilityIdentifier("finishPairing")
                        Text("Checks the original machine with this phone's saved key. It never registers another key.")
                            .font(.caption).foregroundStyle(.secondary)
                        Button("Discard unfinished pairing", role: .destructive) { discardPairing = true }.disabled(model.connecting)
                    }
                    #if DEBUG
                    if ProcessInfo.processInfo.arguments.contains("--ssh-integration-test"),
                       let code = ProcessInfo.processInfo.environment["PIKA_UI_TEST_PAIRING_QR"] {
                        Button("Import disposable pairing code") { model.beginPairing(code) }.accessibilityIdentifier("importPairingCode")
                    }
                    #endif
                }
                DisclosureGroup("Manual login", isExpanded: $manual) {
                Text("Ordinary SSH access is required. Tailscale visibility alone does not grant login.")
                    .font(.caption).foregroundStyle(.secondary)
                Section("Machine address") {
                    TextField("Address or Tailscale name", text: $address).textContentType(.URL).textInputAutocapitalization(.never).autocorrectionDisabled().accessibilityIdentifier("machineAddress")
                    TextField("Username", text: $username).textContentType(.username).textInputAutocapitalization(.never).autocorrectionDisabled()
                    TextField("SSH port", text: $port).keyboardType(.numberPad)
                }
                Section("Authenticate once") {
                    Picker("Login method", selection: $keyMode) { Text("Password").tag(false); Text("Import key").tag(true) }.pickerStyle(.segmented)
                    if keyMode {
                        Button(keyName ?? "Choose an OpenSSH Ed25519 private key") { importKey = true }
                        #if DEBUG
                        if ProcessInfo.processInfo.arguments.contains("--ssh-integration-test"),
                            let encoded = ProcessInfo.processInfo.environment["PIKA_UI_TEST_KEY_BASE64"] {
                            Button("Import disposable integration key") {
                                guard let data = Data(base64Encoded: encoded), data.count <= 65_536 else { return }
                                keyBytes = data; keyName = "Disposable test key"
                            }.accessibilityIdentifier("importIntegrationKey")
                        }
                        #endif
                        SecureField("Key passphrase, if encrypted", text: $passphrase)
                        Text("This alpha verifies Ed25519 keys. Unsupported formats are not converted or silently weakened.").font(.caption).foregroundStyle(.secondary)
                    } else { SecureField("SSH password", text: $password).textContentType(.password) }
                    Text("Credentials stay in this iPhone's Keychain after authentication and machine verification succeed.").font(.caption).foregroundStyle(.secondary)
                }
                if let importError { Section { Text(importError).foregroundStyle(.secondary).font(.callout) } }
                if model.notice != nil { Section { NoticeCard() } }
                Section {
                    Button(model.connecting ? "Connecting…" : "Connect and verify machine") {
                        guard let port = Int(port) else { importError = "Enter a valid SSH port."; return }
                        model.beginConnection(address: address.trimmingCharacters(in: .whitespacesAndNewlines), port: port,
                            username: username, secret: keyMode ? (keyBytes ?? Data()) : Data(password.utf8), key: keyMode, passphrase: passphrase)
                    }.disabled(model.connecting || address.isEmpty || username.isEmpty || (keyMode ? keyBytes == nil : password.isEmpty))
                        .accessibilityIdentifier("connectMachine")
                }
                }
                if !model.machines.isEmpty {
                    Section("Saved machines") {
                        ForEach(model.machines) { machine in
                            VStack(alignment: .leading) {
                                Text(machine.address)
                                Text(machine.username).font(.caption).foregroundStyle(.secondary)
                                DisclosureGroup("Details") { Text("Verified Pika identity: \(machine.id)").font(.caption.monospaced()).textSelection(.enabled) }.font(.caption)
                            }
                        }
                    }
                }
            }.navigationTitle("Add a machine")
                .toolbar { ToolbarItem(placement: .cancellationAction) { Button("Cancel") { if model.connecting { model.cancelConnection() }; dismiss() } } }
                .fileImporter(isPresented: $importKey, allowedContentTypes: [.data]) { result in
                    do {
                        let url = try result.get()
                        let access = url.startAccessingSecurityScopedResource()
                        defer { if access { url.stopAccessingSecurityScopedResource() } }
                        guard let size = try url.resourceValues(forKeys: [.fileSizeKey]).fileSize, size <= 65_536 else { throw ConnectionError.credentials }
                        keyBytes = try Data(contentsOf: url); keyName = url.lastPathComponent; importError = nil
                    } catch { importError = "The selected key could not be imported. It must be a small OpenSSH Ed25519 private key." }
                }
                .sheet(isPresented: $scan, onDismiss: {
                    guard let code = scannedCode else { return }
                    scannedCode = nil
                    model.beginPairing(code)
                }) {
                    NavigationStack {
                        PairingScanner(scanned: { code in scannedCode = code; scan = false },
                            failed: { message in scan = false; importError = message; model.notice = message })
                            .ignoresSafeArea(edges: .bottom).navigationTitle("Scan pairing code")
                            .toolbar { ToolbarItem(placement: .cancellationAction) { Button("Cancel") { scan = false } } }
                    }
                }
                .alert("Discard this phone's unfinished pairing?", isPresented: $discardPairing) {
                    Button("Discard", role: .destructive) { model.discardPairing() }
                    Button("Keep pairing", role: .cancel) {}
                } message: {
                    Text("The key may already be authorized on the machine. This removes the phone's saved key, not the machine's authorization.")
                }
                .sheet(item: $model.hostChallenge) { challenge in
                    VStack(alignment: .leading, spacing: 20) {
                        Text("Verify this machine").font(.title2.bold())
                        Text(challenge.address).font(.headline)
                        Text("Compare this SSH fingerprint with a trusted record for the machine. A network connection alone does not establish its identity.")
                        Text(challenge.fingerprint).font(.system(.callout, design: .monospaced)).textSelection(.enabled)
                        Button("I verified this fingerprint") { model.verifyHost(true) }.buttonStyle(PikaPrimaryButtonStyle())
                        Button("Cancel", role: .cancel) { model.verifyHost(false) }
                    }.padding(28).presentationDetents([.medium, .large]).interactiveDismissDisabled()
                }
                .scrollContentBackground(.hidden)
                .background(PikaTheme.background)
                .onChange(of: model.connected) { _, connected in if connected { password = ""; passphrase = ""; keyBytes = nil; dismiss() } }
                .onChange(of: scenePhase) { _, phase in if phase == .background { scannedCode = nil; scan = false; if model.connecting { model.cancelConnection() } } }
        }
    }
}
