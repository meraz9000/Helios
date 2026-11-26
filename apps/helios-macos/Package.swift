// swift-tools-version: 5.10
import PackageDescription

let package = Package(
    name: "HeliosMacOS",
    platforms: [
        .macOS(.v14)
    ],
    products: [
        .executable(name: "HeliosViewer", targets: ["HeliosMacOS"])
    ],
    targets: [
        .systemLibrary(
            name: "CHeliosClient",
            path: "Sources/CHeliosClient"
        ),
        .executableTarget(
            name: "HeliosMacOS",
            dependencies: ["CHeliosClient"],
            path: "Sources/HeliosMacOS",
            linkerSettings: [
                .unsafeFlags(["-L../../target/release", "-lhelios_client_core"]),
                .linkedFramework("Metal"),
                .linkedFramework("MetalKit"),
                .linkedFramework("VideoToolbox"),
                .linkedFramework("CoreMedia"),
                .linkedFramework("CoreVideo"),
                .linkedFramework("AppKit"),
                .linkedFramework("Security"),
                .linkedFramework("SystemConfiguration"),
            ]
        )
    ]
)
