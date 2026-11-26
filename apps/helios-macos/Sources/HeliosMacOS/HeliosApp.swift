import AppKit
import Metal
import SwiftUI

enum HeliosQualityMode: UInt8, CaseIterable, Identifiable {
    case auto = 0
    case survivalText = 1
    case lowBandwidth = 2
    case balanced = 3
    case smooth = 4

    var id: UInt8 { rawValue }

    var label: String {
        switch self {
        case .auto: return "Auto"
        case .survivalText: return "Survival Text"
        case .lowBandwidth: return "Low-BW"
        case .balanced: return "Balanced"
        case .smooth: return "Smooth"
        }
    }
}

@MainActor
final class HeliosSessionViewModel: ObservableObject {
    @Published var relayURL: String = "wss://137-184-27-200.sslip.io/ws"
    @Published var targetHostID: String = "" {
        didSet {
            if !targetHostID.isEmpty,
               let saved = UserDefaults.standard.string(forKey: "helios.passcode.\(targetHostID)"),
               accessPasscode.isEmpty
            {
                accessPasscode = saved
            }
        }
    }
    @Published var accessPasscode: String = ""
    @Published var onlineHosts: [OnlineHostInfo] = []
    @Published var selectedMode: HeliosQualityMode = .auto {
        didSet { bridge.setQualityMode(selectedMode.rawValue) }
    }
    @Published var mapCommandToCtrl: Bool = true
    @Published var autoReconnect: Bool = true
    @Published var activeMonitor: UInt32 = 0
    @Published var remoteCursor: NSCursor?
    @Published var isConnected: Bool = false
    @Published var connectionPath: String = "Idle"
    @Published var rttMs: Double = 0
    @Published var bitrateKbps: Int = 0
    @Published var fps: Double = 0
    @Published var statusMessage: String = "Looking for online Windows Hosts..."

    let renderer: HeliosMetalRenderer?
    let bridge: HeliosRustBridge

    private var userRequestedDisconnect = false
    private var wantConnected = false
    private var reconnectDelaySecs: Double = 2.0
    private var reconnectWorkItem: DispatchWorkItem?
    private var wakeObserver: NSObjectProtocol?

    init() {
        let device = MTLCreateSystemDefaultDevice()
        self.renderer = device.flatMap { HeliosMetalRenderer(device: $0) }
        self.bridge = HeliosRustBridge(renderer: self.renderer)

        self.bridge.onTelemetry = { [weak self] t in
            DispatchQueue.main.async {
                guard let self else { return }
                self.rttMs = Double(t.rtt_ms)
                self.bitrateKbps = Int(t.video_kbps)
                self.fps = Double(t.fps)
                self.connectionPath = t.using_relay ? "QUIC Relay (DO)" : "QUIC Direct P2P"
            }
        }

        self.bridge.onCursorShape = { [weak self] cursor in
            DispatchQueue.main.async {
                self?.remoteCursor = cursor
            }
        }

        self.bridge.onClipboardReceived = { [weak self] text in
            DispatchQueue.main.async {
                let pb = NSPasteboard.general
                pb.clearContents()
                pb.setString(text, forType: .string)
                self?.statusMessage = "Pulled \(text.count) clipboard chars from Windows Host"
            }
        }

        self.bridge.onStateChange = { [weak self] code, msg in
            DispatchQueue.main.async {
                guard let self else { return }
                self.statusMessage = msg
                switch code {
                case 2:
                    self.isConnected = true
                    self.reconnectDelaySecs = 2.0
                    self.connectionPath = "QUIC Direct P2P"
                    if !self.targetHostID.isEmpty && !self.accessPasscode.isEmpty {
                        UserDefaults.standard.set(self.accessPasscode, forKey: "helios.passcode.\(self.targetHostID)")
                    }
                case 3:
                    self.isConnected = true
                    self.reconnectDelaySecs = 2.0
                    self.connectionPath = "QUIC Relay (DO)"
                    if !self.targetHostID.isEmpty && !self.accessPasscode.isEmpty {
                        UserDefaults.standard.set(self.accessPasscode, forKey: "helios.passcode.\(self.targetHostID)")
                    }
                case 1:
                    self.connectionPath = "Connecting..."
                default:
                    self.isConnected = false
                    self.remoteCursor = nil
                    self.connectionPath = "Idle"
                    self.rttMs = 0
                    self.bitrateKbps = 0
                    self.fps = 0
                    let isBadPasscode = msg.localizedCaseInsensitiveContains("Passcode")
                    if !isBadPasscode && self.autoReconnect && self.wantConnected && !self.userRequestedDisconnect {
                        self.scheduleReconnect()
                    }
                }
            }
        }

        // Automatically re-establish QUIC session after macOS wakes from sleep
        self.wakeObserver = NSWorkspace.shared.notificationCenter.addObserver(
            forName: NSWorkspace.didWakeNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            Task { @MainActor in
                guard let self, self.autoReconnect, self.wantConnected else { return }
                self.statusMessage = "macOS woke from sleep — reconnecting QUIC session..."
                self.reconnectNow()
            }
        }

        // Auto-discover online Windows Hosts immediately on launch
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { [weak self] in
            self?.refreshHosts()
        }
    }

    deinit {
        if let obs = wakeObserver {
            NSWorkspace.shared.notificationCenter.removeObserver(obs)
        }
    }

    func refreshHosts() {
        let url = relayURL
        statusMessage = "Looking for online Windows Hosts..."
        DispatchQueue.global(qos: .userInitiated).async {
            let hosts = HeliosRustBridge.listOnlineHosts(relayURL: url, pairingToken: "")
            DispatchQueue.main.async {
                self.onlineHosts = hosts
                if let first = hosts.first, self.targetHostID.isEmpty {
                    self.targetHostID = first.device_id
                }
                self.statusMessage = hosts.isEmpty
                    ? "No online Windows Hosts yet — double-click helios-host.exe on your Windows PC"
                    : "Found \(hosts.count) online Host(s) — enter Access Passcode and click Connect"
            }
        }
    }

    func toggleConnect() {
        reconnectWorkItem?.cancel()
        reconnectWorkItem = nil

        if isConnected || wantConnected {
            userRequestedDisconnect = true
            wantConnected = false
            bridge.disconnect()
            isConnected = false
            remoteCursor = nil
            connectionPath = "Idle"
            statusMessage = "Disconnected"
            return
        }

        guard !targetHostID.isEmpty else {
            statusMessage = "Select an online Windows Host or click Refresh first"
            return
        }

        guard !accessPasscode.isEmpty else {
            statusMessage = "Enter the Host Access Passcode shown by helios-host.exe"
            return
        }

        userRequestedDisconnect = false
        wantConnected = true
        reconnectDelaySecs = 2.0
        startConnection()
    }

    private func startConnection() {
        let ok = bridge.connect(
            relayURL: relayURL,
            hostID: targetHostID,
            accessPasscode: accessPasscode,
            modeRaw: selectedMode.rawValue
        )
        if ok {
            statusMessage = "Connecting to \(targetHostID) via QUIC..."
        } else {
            statusMessage = "Failed to start Helios QUIC client"
            if autoReconnect && wantConnected {
                scheduleReconnect()
            }
        }
    }

    private func scheduleReconnect() {
        reconnectWorkItem?.cancel()
        let delay = reconnectDelaySecs
        reconnectDelaySecs = min(reconnectDelaySecs * 2.0, 15.0)
        statusMessage = "Connection dropped — auto-reconnecting in \(Int(delay))s..."

        let item = DispatchWorkItem { [weak self] in
            Task { @MainActor in
                guard let self, self.wantConnected, !self.userRequestedDisconnect else { return }
                self.startConnection()
            }
        }
        reconnectWorkItem = item
        DispatchQueue.main.asyncAfter(deadline: .now() + delay, execute: item)
    }

    private func reconnectNow() {
        reconnectWorkItem?.cancel()
        reconnectDelaySecs = 2.0
        bridge.disconnect()
        startConnection()
    }

    func syncClipboardToWindows() {
        if let text = NSPasteboard.general.string(forType: .string), !text.isEmpty {
            bridge.sendClipboard(text)
            statusMessage = "Sent \(text.count) clipboard chars to Windows Host"
        }
    }

    func pullClipboardFromWindows() {
        bridge.requestClipboard()
    }

    func cycleMonitor() {
        activeMonitor = (activeMonitor + 1) % 4
        bridge.switchMonitor(activeMonitor)
        statusMessage = "Requested Windows Monitor #\(activeMonitor)"
    }
}

@main
struct HeliosMacOSApp: App {
    @StateObject private var vm = HeliosSessionViewModel()

    var body: some Scene {
        WindowGroup("Helios Remote Desktop") {
            VStack(spacing: 0) {
                // Connection & Host Discovery Bar
                HStack(spacing: 8) {
                    TextField("Relay WSS URL", text: $vm.relayURL)
                        .textFieldStyle(.roundedBorder)
                        .frame(minWidth: 200)

                    if !vm.onlineHosts.isEmpty {
                        Picker("", selection: $vm.targetHostID) {
                            ForEach(vm.onlineHosts) { h in
                                Text("\(h.name) (\(h.device_id))").tag(h.device_id)
                            }
                        }
                        .frame(width: 210)
                    } else {
                        TextField("Host ID (HL-XXXX-XXXX)", text: $vm.targetHostID)
                            .textFieldStyle(.roundedBorder)
                            .frame(width: 170)
                    }

                    Button("Refresh") { vm.refreshHosts() }

                    SecureField("Host Access Passcode", text: $vm.accessPasscode)
                        .textFieldStyle(.roundedBorder)
                        .frame(width: 170)

                    Button(vm.isConnected ? "Disconnect" : "Connect") {
                        vm.toggleConnect()
                    }
                    .buttonStyle(.borderedProminent)
                    .tint(vm.isConnected ? .red : .accentColor)
                }
                .padding(.horizontal, 10)
                .padding(.top, 8)
                .padding(.bottom, 6)
                .background(.ultraThinMaterial)

                // Low-Bandwidth Quality & Remote Action Bar
                HStack(spacing: 10) {
                    Picker("Quality", selection: $vm.selectedMode) {
                        ForEach(HeliosQualityMode.allCases) { mode in
                            Text(mode.label).tag(mode)
                        }
                    }
                    .pickerStyle(.segmented)
                    .frame(maxWidth: 420)

                    Toggle("Cmd→Ctrl", isOn: $vm.mapCommandToCtrl)
                        .toggleStyle(.checkbox)

                    Toggle("Auto-Reconnect", isOn: $vm.autoReconnect)
                        .toggleStyle(.checkbox)

                    Spacer()

                    Button("Monitor \(vm.activeMonitor)") { vm.cycleMonitor() }
                        .disabled(!vm.isConnected)

                    Button("Push Clip") { vm.syncClipboardToWindows() }
                        .disabled(!vm.isConnected)

                    Button("Pull Clip") { vm.pullClipboardFromWindows() }
                        .disabled(!vm.isConnected)

                    Button("Ctrl+Alt+Del") { vm.bridge.sendCtrlAltDel() }
                        .disabled(!vm.isConnected)
                }
                .padding(.horizontal, 10)
                .padding(.bottom, 8)
                .background(.ultraThinMaterial)

                Divider()

                // Live Metal + VideoToolbox Remote Desktop Viewport with Local Hardware Cursor
                ZStack {
                    MetalViewportRepresentable(
                        renderer: vm.renderer,
                        remoteCursor: vm.remoteCursor,
                        mapCommandToCtrl: vm.mapCommandToCtrl,
                        onMouseMoveAbs: { x, y in vm.bridge.sendMouseAbs(x: x, y: y) },
                        onMouseButton: { btn, pressed in vm.bridge.sendMouseButton(button: btn, pressed: pressed) },
                        onMouseWheel: { dx, dy in vm.bridge.sendMouseWheel(dx: dx, dy: dy) },
                        onKey: { vk, pressed in vm.bridge.sendKey(vk: vk, pressed: pressed) }
                    )

                    if !vm.isConnected {
                        VStack(spacing: 8) {
                            Image(systemName: "desktopcomputer")
                                .font(.system(size: 40))
                                .foregroundStyle(.secondary)
                            Text(vm.statusMessage)
                                .font(.headline)
                                .multilineTextAlignment(.center)
                        }
                        .padding(24)
                        .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 12))
                    }
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)

                Divider()

                // Live Telemetry Footer
                HStack(spacing: 16) {
                    Label(vm.connectionPath, systemImage: vm.isConnected ? "bolt.fill" : "bolt.slash")
                        .foregroundStyle(vm.isConnected ? .green : .secondary)
                    Text(String(format: "RTT: %.1f ms", vm.rttMs))
                    Text("Bitrate: \(vm.bitrateKbps) kbps")
                    Text(String(format: "FPS: %.1f", vm.fps))
                    Spacer()
                    Text(vm.statusMessage)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
                .font(.caption.monospacedDigit())
                .padding(.horizontal, 12)
                .padding(.vertical, 6)
                .background(.bar)
            }
            .frame(minWidth: 1080, minHeight: 680)
        }
    }
}
