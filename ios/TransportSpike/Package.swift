// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "PikaTransportSpike",
    platforms: [.macOS(.v15), .iOS(.v17)],
    dependencies: [.package(url: "https://github.com/orlandos-nl/Citadel.git", exact: "0.12.1")],
    targets: [.executableTarget(name: "TransportSpike", dependencies: [.product(name: "Citadel", package: "Citadel")])]
)
