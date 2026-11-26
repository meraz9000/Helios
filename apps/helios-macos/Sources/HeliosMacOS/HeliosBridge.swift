import AppKit
import CHeliosClient
import CoreGraphics
import Foundation

struct OnlineHostInfo: Decodable, Identifiable, Hashable {
    let device_id: String
    let name: String
    let os: String
    let online: Bool
    let active_sessions: UInt32

    var id: String { device_id }
}

/// Safe Swift bridge owning an active Rust `HeliosClientHandle` and forwarding
/// RGBA dirty-tile frames, H.264/HEVC VideoToolbox frames, local Windows cursor bitmaps,
/// clipboard pull responses, and live telemetry callbacks.
final class HeliosRustBridge {
    private var handle: OpaquePointer?
    let renderer: HeliosMetalRenderer?
    let videoDecoder = VideoToolboxDecoder()

    var onTelemetry: ((HeliosTelemetryC) -> Void)?
    var onStateChange: ((UInt8, String) -> Void)?
    var onCursorShape: ((NSCursor) -> Void)?
    var onClipboardReceived: ((String) -> Void)?

    init(renderer: HeliosMetalRenderer?) {
        self.renderer = renderer
        self.videoDecoder.onDecodedPixelBuffer = { [weak renderer] pixelBuffer in
            renderer?.updatePixelBuffer(pixelBuffer)
        }
    }

    deinit {
        disconnect()
    }

    static func defaultIdentityPath() -> String {
        let appSupport = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first!
        let dir = appSupport.appendingPathComponent("Helios", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir.appendingPathComponent("client-identity.json").path
    }

    static func listOnlineHosts(relayURL: String, pairingToken: String) -> [OnlineHostInfo] {
        let idPath = defaultIdentityPath()
        let hostName = Host.current().localizedName ?? "MacBook"
        guard let cPtr = helios_client_list_hosts(
            relayURL,
            idPath,
            hostName,
            pairingToken.isEmpty ? nil : pairingToken
        ) else {
            return []
        }
        defer { helios_string_free(cPtr) }
        let jsonStr = String(cString: cPtr)
        guard let data = jsonStr.data(using: .utf8),
              let decoded = try? JSONDecoder().decode([OnlineHostInfo].self, from: data)
        else {
            return []
        }
        return decoded
    }

    func connect(
        relayURL: String,
        hostID: String,
        pairingToken: String,
        modeRaw: UInt8
    ) -> Bool {
        disconnect()

        let idPath = Self.defaultIdentityPath()
        let clientName = Host.current().localizedName ?? "MacBook"
        let selfPtr = Unmanaged.passUnretained(self).toOpaque()

        let h = helios_client_connect(
            relayURL,
            idPath,
            clientName,
            pairingToken.isEmpty ? nil : pairingToken,
            hostID,
            modeRaw,
            selfPtr,
            { userData, width, height, rgbaPtr, rgbaLen, telemetry in
                guard let userData, let rgbaPtr, rgbaLen > 0 else { return }
                let bridge = Unmanaged<HeliosRustBridge>.fromOpaque(userData).takeUnretainedValue()
                bridge.renderer?.updateRGBA(width: Int(width), height: Int(height), bytes: rgbaPtr)
                bridge.onTelemetry?(telemetry)
            },
            { userData, _, _, codec, _, tsUs, dataPtr, dataLen, telemetry in
                guard let userData, let dataPtr, dataLen > 0 else { return }
                let bridge = Unmanaged<HeliosRustBridge>.fromOpaque(userData).takeUnretainedValue()
                let data = Data(bytes: dataPtr, count: Int(dataLen))
                bridge.videoDecoder.decodeAnnexB(data, codec: codec, timestampUs: tsUs)
                bridge.onTelemetry?(telemetry)
            },
            { userData, stateCode, cMsg in
                guard let userData else { return }
                let bridge = Unmanaged<HeliosRustBridge>.fromOpaque(userData).takeUnretainedValue()
                let msg = cMsg.map { String(cString: $0) } ?? ""
                bridge.onStateChange?(stateCode, msg)
            },
            { userData, width, height, hotspotX, hotspotY, rgbaPtr, rgbaLen in
                guard let userData, let rgbaPtr, rgbaLen > 0, width > 0, height > 0 else { return }
                let bridge = Unmanaged<HeliosRustBridge>.fromOpaque(userData).takeUnretainedValue()
                let data = Data(bytes: rgbaPtr, count: Int(rgbaLen))
                if let cursor = Self.makeNSCursor(
                    width: Int(width),
                    height: Int(height),
                    hotspotX: Int(hotspotX),
                    hotspotY: Int(hotspotY),
                    rgba: data
                ) {
                    bridge.onCursorShape?(cursor)
                }
            },
            { userData, cText in
                guard let userData, let cText else { return }
                let bridge = Unmanaged<HeliosRustBridge>.fromOpaque(userData).takeUnretainedValue()
                bridge.onClipboardReceived?(String(cString: cText))
            }
        )

        self.handle = h
        return h != nil
    }

    private static func makeNSCursor(
        width: Int,
        height: Int,
        hotspotX: Int,
        hotspotY: Int,
        rgba: Data
    ) -> NSCursor? {
        guard rgba.count == width * height * 4,
              let provider = CGDataProvider(data: rgba as CFData)
        else { return nil }

        let colorSpace = CGColorSpaceCreateDeviceRGB()
        let bitmapInfo = CGBitmapInfo(rawValue: CGImageAlphaInfo.last.rawValue)
        guard let cgImage = CGImage(
            width: width,
            height: height,
            bitsPerComponent: 8,
            bitsPerPixel: 32,
            bytesPerRow: width * 4,
            space: colorSpace,
            bitmapInfo: bitmapInfo,
            provider: provider,
            decode: nil,
            shouldInterpolate: false,
            intent: .defaultIntent
        ) else { return nil }

        let nsImage = NSImage(cgImage: cgImage, size: NSSize(width: width, height: height))
        return NSCursor(image: nsImage, hotSpot: NSPoint(x: hotspotX, y: hotspotY))
    }

    func disconnect() {
        if let h = handle {
            helios_client_disconnect(h)
            handle = nil
        }
        videoDecoder.invalidate()
    }

    func sendMouseAbs(x: Float, y: Float) {
        if let h = handle {
            _ = helios_client_send_mouse_abs(h, x, y)
        }
    }

    func sendMouseButton(button: UInt8, pressed: Bool) {
        if let h = handle {
            _ = helios_client_send_mouse_button(h, button, pressed)
        }
    }

    func sendMouseWheel(dx: Int32, dy: Int32) {
        if let h = handle {
            _ = helios_client_send_mouse_wheel(h, dx, dy)
        }
    }

    func sendKey(vk: UInt16, pressed: Bool) {
        if let h = handle {
            _ = helios_client_send_key(h, vk, pressed)
        }
    }

    func sendCtrlAltDel() {
        if let h = handle {
            _ = helios_client_send_ctrl_alt_del(h)
        }
    }

    func setQualityMode(_ modeRaw: UInt8) {
        if let h = handle {
            _ = helios_client_set_quality_mode(h, modeRaw)
        }
    }

    func sendClipboard(_ text: String) {
        if let h = handle {
            _ = helios_client_send_clipboard(h, text)
        }
    }

    func requestClipboard() {
        if let h = handle {
            _ = helios_client_request_clipboard(h)
        }
    }

    func switchMonitor(_ index: UInt32) {
        if let h = handle {
            _ = helios_client_switch_monitor(h, index)
        }
    }
}
