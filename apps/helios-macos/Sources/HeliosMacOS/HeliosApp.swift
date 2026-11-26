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

    var shortLabel: String {
        switch self {
        case .auto: return "Auto"
        case .survivalText: return "Text"
        case .lowBandwidth: return "Low"
        case .balanced: return "Bal"
        case .smooth: return "Smooth"
        }
    }

    var fullLabel: String {
        switch self {
        case .auto: return "Auto Adaptive"
        case .survivalText: return "Survival Text (<150 kbps)"
        case .lowBandwidth: return "Low Bandwidth (720p)"
        case .balanced: return "Balanced (1080p)"
        case .smooth: return "Smooth (60 FPS)"
        }
    }
}

@MainActor
final class HeliosSessionViewModel: ObservableObject {
    @Published var relayURL: String = "wss://137-184-27-200.sslip.io/ws"
    @Published var targetHostID: String = "" {
        didSet {
            if !targetHostID.isEmpty,
               let saved = UserDefaults.standard.string(forKey: "helios.passcode.\(targetHostID)")
            {
                accessPasscode = saved
            }
        }
    }
    @Published var accessPasscode: String = ""
    @Published var onlineHosts: [OnlineHostInfo] = []
    @Published var isRefreshingHosts: Bool = false
    @Published var selectedMode: HeliosQualityMode = .auto {
        didSet { bridge.setQualityMode(selectedMode.rawValue) }
    }
    @Published var mapCommandToCtrl: Bool = true
    @Published var autoReconnect: Bool = true
    @Published var hudCollapsed: Bool = false
    @Published var showServerSettings: Bool = false
    @Published var activeMonitor: UInt32 = 0
    @Published var remoteCursor: NSCursor?
    @Published var isConnected: Bool = false
    @Published var isConnecting: Bool = false
    @Published var isUsingRelay: Bool = false
    @Published var connectionPath: String = "Idle"
    @Published var rttMs: Double = 0
    @Published var bitrateKbps: Int = 0
    @Published var fps: Double = 0
    @Published var statusMessage: String = "Scanning for your Windows PC..."

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

        if let savedRelay = UserDefaults.standard.string(forKey: "helios.relayURL"), !savedRelay.isEmpty {
            self.relayURL = savedRelay
        }

        self.bridge.onTelemetry = { [weak self] t in
            DispatchQueue.main.async {
                guard let self else { return }
                self.rttMs = Double(t.rtt_ms)
                self.bitrateKbps = Int(t.video_kbps)
                self.fps = Double(t.fps)
                self.isUsingRelay = t.using_relay
                self.connectionPath = t.using_relay ? "Relay" : "Direct P2P"
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
                self?.statusMessage = "Pulled \(text.count) clipboard chars from Windows"
            }
        }

        self.bridge.onStateChange = { [weak self] code, msg in
            DispatchQueue.main.async {
                guard let self else { return }
                self.statusMessage = msg
                switch code {
                case 2:
                    self.isConnected = true
                    self.isConnecting = false
                    self.isUsingRelay = false
                    self.reconnectDelaySecs = 2.0
                    self.connectionPath = "Direct P2P"
                    self.saveCurrentPasscode()
                case 3:
                    self.isConnected = true
                    self.isConnecting = false
                    self.isUsingRelay = true
                    self.reconnectDelaySecs = 2.0
                    self.connectionPath = "Relay"
                    self.saveCurrentPasscode()
                case 1:
                    self.isConnecting = true
                    self.connectionPath = "Connecting..."
                default:
                    self.isConnected = false
                    self.isConnecting = false
                    self.remoteCursor = nil
                    self.connectionPath = "Idle"
                    self.rttMs = 0
                    self.bitrateKbps = 0
                    self.fps = 0
                    let isBadPasscode = msg.localizedCaseInsensitiveContains("Passcode")
                    if !isBadPasscode && self.autoReconnect && self.wantConnected && !self.userRequestedDisconnect {
                        self.scheduleReconnect()
                    } else if isBadPasscode {
                        self.wantConnected = false
                    }
                }
            }
        }

        // Auto-reconnect when macOS wakes from sleep
        self.wakeObserver = NSWorkspace.shared.notificationCenter.addObserver(
            forName: NSWorkspace.didWakeNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            Task { @MainActor in
                guard let self, self.autoReconnect, self.wantConnected else { return }
                self.statusMessage = "macOS woke from sleep — reconnecting..."
                self.reconnectNow()
            }
        }

        DispatchQueue.main.asyncAfter(deadline: .now() + 0.25) { [weak self] in
            self?.refreshHosts()
        }
    }

    deinit {
        if let obs = wakeObserver {
            NSWorkspace.shared.notificationCenter.removeObserver(obs)
        }
    }

    private func saveCurrentPasscode() {
        UserDefaults.standard.set(relayURL, forKey: "helios.relayURL")
        if !targetHostID.isEmpty && !accessPasscode.isEmpty {
            UserDefaults.standard.set(accessPasscode, forKey: "helios.passcode.\(targetHostID)")
        }
    }

    func selectHost(_ host: OnlineHostInfo) {
        targetHostID = host.device_id
    }

    func refreshHosts() {
        let url = relayURL
        isRefreshingHosts = true
        statusMessage = "Scanning relay for online Windows PCs..."
        DispatchQueue.global(qos: .userInitiated).async {
            let hosts = HeliosRustBridge.listOnlineHosts(relayURL: url, pairingToken: "")
            DispatchQueue.main.async {
                self.isRefreshingHosts = false
                self.onlineHosts = hosts
                if let first = hosts.first, self.targetHostID.isEmpty {
                    self.targetHostID = first.device_id
                }
                self.statusMessage = hosts.isEmpty
                    ? "No online Windows Hosts yet — double-click helios-host.exe on your PC"
                    : "\(hosts.count) Windows Host\(hosts.count == 1 ? "" : "s") online and ready"
            }
        }
    }

    func connectOrDisconnect() {
        reconnectWorkItem?.cancel()
        reconnectWorkItem = nil

        if isConnected || wantConnected {
            userRequestedDisconnect = true
            wantConnected = false
            isConnecting = false
            bridge.disconnect()
            isConnected = false
            remoteCursor = nil
            connectionPath = "Idle"
            statusMessage = "Disconnected"
            refreshHosts()
            return
        }

        guard !targetHostID.isEmpty else {
            statusMessage = "Select an online Windows PC or enter its Host ID"
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
        isConnecting = true
        let ok = bridge.connect(
            relayURL: relayURL,
            hostID: targetHostID,
            accessPasscode: accessPasscode,
            modeRaw: selectedMode.rawValue
        )
        if ok {
            statusMessage = "Negotiating QUIC TLS 1.3 tunnel to \(targetHostID)..."
        } else {
            isConnecting = false
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
        statusMessage = "Connection dropped — reconnecting in \(Int(delay))s..."

        let item = DispatchWorkItem { [weak self] in
            Task { @MainActor in
                guard let self, self.wantConnected, !self.userRequestedDisconnect else { return }
                self.startConnection()
            }
        }
        reconnectWorkItem = item
        DispatchQueue.main.asyncAfter(deadline: .now() + delay, execute: item)
    }

    func reconnectNow() {
        reconnectWorkItem?.cancel()
        reconnectDelaySecs = 2.0
        bridge.disconnect()
        startConnection()
    }

    func syncClipboardToWindows() {
        if let text = NSPasteboard.general.string(forType: .string), !text.isEmpty {
            bridge.sendClipboard(text)
            statusMessage = "Pushed \(text.count) clipboard chars to Windows"
        }
    }

    func pullClipboardFromWindows() {
        bridge.requestClipboard()
    }

    func cycleMonitor() {
        activeMonitor = (activeMonitor + 1) % 4
        bridge.switchMonitor(activeMonitor)
        statusMessage = "Switched to Windows Monitor #\(activeMonitor)"
    }
}

// MARK: - Lobby View (Liquid Glass Landing Screen)

private struct HeliosLobbyView: View {
    @ObservedObject var vm: HeliosSessionViewModel

    var body: some View {
        ZStack {
            // Deep obsidian-indigo ambient backdrop with soft warm solar radiance
            LinearGradient(
                colors: [
                    Color(red: 0.05, green: 0.07, blue: 0.12),
                    Color(red: 0.08, green: 0.10, blue: 0.17),
                    Color(red: 0.04, green: 0.05, blue: 0.09),
                ],
                startPoint: .topLeading,
                endPoint: .bottomTrailing
            )
            .ignoresSafeArea()

            Circle()
                .fill(
                    RadialGradient(
                        colors: [
                            Color.orange.opacity(0.26),
                            Color.amberGlow.opacity(0.10),
                            Color.clear,
                        ],
                        center: .center,
                        startRadius: 10,
                        endRadius: 340
                    )
                )
                .frame(width: 680, height: 680)
                .offset(x: -220, y: -170)
                .blur(radius: 28)

            Circle()
                .fill(
                    RadialGradient(
                        colors: [
                            Color.cyan.opacity(0.18),
                            Color.blue.opacity(0.08),
                            Color.clear,
                        ],
                        center: .center,
                        startRadius: 10,
                        endRadius: 320
                    )
                )
                .frame(width: 640, height: 640)
                .offset(x: 240, y: 180)
                .blur(radius: 34)

            // Main floating Liquid Glass card
            VStack(alignment: .leading, spacing: 20) {
                // Header
                HStack(spacing: 14) {
                    ZStack {
                        Circle()
                            .fill(
                                LinearGradient(
                                    colors: [Color.orange, Color(red: 0.96, green: 0.45, blue: 0.15)],
                                    startPoint: .topLeading,
                                    endPoint: .bottomTrailing
                                )
                            )
                            .frame(width: 44, height: 44)
                            .shadow(color: .orange.opacity(0.45), radius: 12, y: 4)
                        Image(systemName: "sun.max.fill")
                            .font(.system(size: 21, weight: .bold))
                            .foregroundStyle(.white)
                    }

                    VStack(alignment: .leading, spacing: 2) {
                        Text("Helios")
                            .font(.system(size: 24, weight: .bold, design: .rounded))
                        Text("Low-Bandwidth QUIC Remote Desktop")
                            .font(.subheadline)
                            .foregroundStyle(.secondary)
                    }

                    Spacer()

                    Button {
                        vm.refreshHosts()
                    } label: {
                        HStack(spacing: 6) {
                            Image(systemName: "arrow.clockwise")
                                .rotationEffect(.degrees(vm.isRefreshingHosts ? 360 : 0))
                                .animation(
                                    vm.isRefreshingHosts
                                        ? .linear(duration: 0.9).repeatForever(autoreverses: false)
                                        : .default,
                                    value: vm.isRefreshingHosts
                                )
                            Text("Refresh")
                        }
                    }
                    .buttonStyle(HeliosGlassButtonStyle())
                }

                // Online Windows PCs Grid / List
                VStack(alignment: .leading, spacing: 10) {
                    HStack {
                        Text("ONLINE WINDOWS HOSTS")
                            .font(.system(size: 11, weight: .semibold))
                            .tracking(0.8)
                            .foregroundStyle(.secondary)
                        Spacer()
                        Text(vm.statusMessage)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                    }

                    if vm.onlineHosts.isEmpty {
                        HStack(spacing: 12) {
                            Image(systemName: "desktopcomputer.trianglebadge.exclamationmark")
                                .font(.system(size: 22))
                                .foregroundStyle(.secondary)
                            VStack(alignment: .leading, spacing: 2) {
                                Text("Waiting for Windows Host...")
                                    .font(.headline)
                                Text("Double-click helios-host.exe on your Windows PC and click Refresh, or type a Host ID below.")
                                    .font(.caption)
                                    .foregroundStyle(.secondary)
                            }
                            Spacer()
                        }
                        .padding(14)
                        .heliosGlassCard(cornerRadius: 14)
                    } else {
                        ScrollView(.horizontal, showsIndicators: false) {
                            HStack(spacing: 12) {
                                ForEach(vm.onlineHosts) { host in
                                    let selected = vm.targetHostID == host.device_id
                                    Button {
                                        vm.selectHost(host)
                                    } label: {
                                        HStack(spacing: 10) {
                                            Image(systemName: "pc")
                                                .font(.system(size: 18, weight: .medium))
                                                .foregroundStyle(selected ? .orange : .primary)
                                            VStack(alignment: .leading, spacing: 2) {
                                                HStack(spacing: 6) {
                                                    Text(host.name)
                                                        .font(.system(size: 13.5, weight: .semibold))
                                                    Circle()
                                                        .fill(.green)
                                                        .frame(width: 7, height: 7)
                                                }
                                                Text(host.device_id)
                                                    .font(.caption.monospaced())
                                                    .foregroundStyle(.secondary)
                                            }
                                        }
                                        .padding(.horizontal, 14)
                                        .padding(.vertical, 10)
                                        .heliosGlassCard(
                                            cornerRadius: 14,
                                            tint: selected ? .orange : nil
                                        )
                                        .overlay {
                                            RoundedRectangle(cornerRadius: 14, style: .continuous)
                                                .stroke(
                                                    selected ? Color.orange.opacity(0.8) : Color.clear,
                                                    lineWidth: 1.5
                                                )
                                        }
                                    }
                                    .buttonStyle(.plain)
                                }
                            }
                        }
                    }
                }

                // Access Passcode & Connect Row
                HStack(spacing: 12) {
                    if vm.onlineHosts.isEmpty {
                        TextField("Host ID (HL-XXXX-XXXX)", text: $vm.targetHostID)
                            .textFieldStyle(.plain)
                            .font(.system(size: 13, design: .monospaced))
                            .padding(.horizontal, 12)
                            .padding(.vertical, 9)
                            .heliosGlassCard(cornerRadius: 12)
                            .frame(width: 185)
                    }

                    HStack(spacing: 8) {
                        Image(systemName: "key.fill")
                            .foregroundStyle(.secondary)
                        SecureField("Host Access Passcode (e.g. K7M9-P2X4-Q8R3)", text: $vm.accessPasscode)
                            .textFieldStyle(.plain)
                            .font(.system(size: 13.5, design: .monospaced))
                            .onSubmit { vm.connectOrDisconnect() }
                    }
                    .padding(.horizontal, 12)
                    .padding(.vertical, 9)
                    .heliosGlassCard(cornerRadius: 12)

                    Button {
                        vm.connectOrDisconnect()
                    } label: {
                        HStack(spacing: 6) {
                            if vm.isConnecting {
                                ProgressView()
                                    .controlSize(.small)
                                    .tint(.white)
                            } else {
                                Image(systemName: "bolt.fill")
                            }
                            Text(vm.isConnecting ? "Connecting" : "Connect")
                        }
                        .frame(minWidth: 100)
                    }
                    .buttonStyle(HeliosGlassButtonStyle(prominent: true, tint: .orange))
                }

                // Quality Preset + Collapsible Relay Server Settings
                HStack(spacing: 12) {
                    Picker("Starting Mode", selection: $vm.selectedMode) {
                        ForEach(HeliosQualityMode.allCases) { mode in
                            Text(mode.shortLabel).tag(mode)
                        }
                    }
                    .pickerStyle(.segmented)
                    .frame(maxWidth: 340)

                    Spacer()

                    Button {
                        withAnimation(.spring(response: 0.25, dampingFraction: 0.85)) {
                            vm.showServerSettings.toggle()
                        }
                    } label: {
                        Label(
                            vm.showServerSettings ? "Hide Server" : "Relay Server",
                            systemImage: "server.rack"
                        )
                    }
                    .buttonStyle(HeliosGlassButtonStyle())
                }

                if vm.showServerSettings {
                    HStack(spacing: 10) {
                        Text("Relay WSS:")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                        TextField("wss://137-184-27-200.sslip.io/ws", text: $vm.relayURL)
                            .textFieldStyle(.plain)
                            .font(.caption.monospaced())
                            .padding(.horizontal, 10)
                            .padding(.vertical, 6)
                            .heliosGlassCard(cornerRadius: 10)
                    }
                    .transition(.opacity.combined(with: .move(edge: .top)))
                }
            }
            .padding(28)
            .frame(maxWidth: 700)
            .heliosGlassCard(cornerRadius: 26)
            .padding(32)
        }
    }
}

// MARK: - Floating Liquid Glass HUD Overlay (Connected Mode)

private struct HeliosFloatingHudBar: View {
    @ObservedObject var vm: HeliosSessionViewModel

    var body: some View {
        VStack(spacing: 6) {
            if vm.hudCollapsed {
                Button {
                    withAnimation(.spring(response: 0.26, dampingFraction: 0.82)) {
                        vm.hudCollapsed = false
                    }
                } label: {
                    HStack(spacing: 8) {
                        Circle()
                            .fill(vm.isUsingRelay ? Color.orange : Color.green)
                            .frame(width: 7, height: 7)
                        Text(String(format: "%.0fms · %dk", vm.rttMs, vm.bitrateKbps))
                            .font(.system(size: 11, weight: .semibold, design: .monospaced))
                        Image(systemName: "chevron.down")
                            .font(.system(size: 10, weight: .bold))
                    }
                    .padding(.horizontal, 12)
                    .padding(.vertical, 5)
                    .heliosGlassPill(interactive: true)
                }
                .buttonStyle(.plain)
            } else {
                HStack(spacing: 10) {
                    // Live Connection Telemetry Badge
                    HStack(spacing: 6) {
                        Circle()
                            .fill(vm.isUsingRelay ? Color.orange : Color.green)
                            .frame(width: 8, height: 8)
                        Text(vm.connectionPath)
                            .font(.system(size: 11.5, weight: .semibold))
                        Text("·")
                            .foregroundStyle(.secondary)
                        Text(String(format: "%.0fms", vm.rttMs))
                        Text(String(format: "%d kbps", vm.bitrateKbps))
                        Text(String(format: "%.0f fps", vm.fps))
                    }
                    .font(.system(size: 11, design: .monospaced))
                    .padding(.horizontal, 10)
                    .padding(.vertical, 5)
                    .background(Color.white.opacity(0.08), in: Capsule())

                    Divider()
                        .frame(height: 18)

                    // Instant Low-Bandwidth Mode Switcher
                    Picker("", selection: $vm.selectedMode) {
                        ForEach(HeliosQualityMode.allCases) { mode in
                            Text(mode.shortLabel).tag(mode)
                        }
                    }
                    .pickerStyle(.segmented)
                    .frame(width: 245)
                    .help("Switch quality preset (choose Text for razor-sharp <150 kbps Survival Mode)")

                    Divider()
                        .frame(height: 18)

                    // Quick Action Glass Icons
                    HeliosHudIconButton(
                        systemImage: "display.2",
                        helpText: "Switch Windows Monitor (Current: #\(vm.activeMonitor))"
                    ) { vm.cycleMonitor() }

                    HeliosHudIconButton(
                        systemImage: "arrow.up.doc",
                        helpText: "Push Mac Clipboard to Windows"
                    ) { vm.syncClipboardToWindows() }

                    HeliosHudIconButton(
                        systemImage: "arrow.down.doc",
                        helpText: "Pull Windows Clipboard to Mac"
                    ) { vm.pullClipboardFromWindows() }

                    HeliosHudIconButton(
                        systemImage: "command",
                        helpText: "Map Command → Ctrl (\(vm.mapCommandToCtrl ? "On" : "Off"))",
                        active: vm.mapCommandToCtrl,
                        tint: .orange
                    ) { vm.mapCommandToCtrl.toggle() }

                    HeliosHudIconButton(
                        systemImage: "lock.shield",
                        helpText: "Send Ctrl+Alt+Del (Unlock Windows)"
                    ) { vm.bridge.sendCtrlAltDel() }

                    HeliosHudIconButton(
                        systemImage: "chevron.up",
                        helpText: "Minimize HUD Bar"
                    ) {
                        withAnimation(.spring(response: 0.26, dampingFraction: 0.82)) {
                            vm.hudCollapsed = true
                        }
                    }

                    Button {
                        vm.connectOrDisconnect()
                    } label: {
                        Image(systemName: "xmark")
                            .font(.system(size: 11, weight: .bold))
                            .foregroundStyle(.white)
                            .frame(width: 26, height: 26)
                            .background(Color.red.opacity(0.82), in: Circle())
                    }
                    .buttonStyle(.plain)
                    .help("Disconnect")
                }
                .padding(.horizontal, 12)
                .padding(.vertical, 7)
                .heliosGlassPill(interactive: true)
            }
        }
        .padding(.top, 10)
    }
}

private extension Color {
    static let amberGlow = Color(red: 1.0, green: 0.68, blue: 0.25)
}

// MARK: - App Entrypoint & macOS Menu Commands

@main
struct HeliosMacOSApp: App {
    @StateObject private var vm = HeliosSessionViewModel()

    var body: some Scene {
        WindowGroup("Helios") {
            ZStack(alignment: .top) {
                if vm.isConnected {
                    MetalViewportRepresentable(
                        renderer: vm.renderer,
                        remoteCursor: vm.remoteCursor,
                        mapCommandToCtrl: vm.mapCommandToCtrl,
                        onMouseMoveAbs: { x, y in vm.bridge.sendMouseAbs(x: x, y: y) },
                        onMouseButton: { btn, pressed in vm.bridge.sendMouseButton(button: btn, pressed: pressed) },
                        onMouseWheel: { dx, dy in vm.bridge.sendMouseWheel(dx: dx, dy: dy) },
                        onKey: { vk, pressed in vm.bridge.sendKey(vk: vk, pressed: pressed) }
                    )
                    .ignoresSafeArea()

                    HeliosFloatingHudBar(vm: vm)
                        .transition(.move(edge: .top).combined(with: .opacity))
                } else {
                    HeliosLobbyView(vm: vm)
                }
            }
            .frame(minWidth: 960, minHeight: 620)
            .preferredColorScheme(.dark)
            .animation(.easeInOut(duration: 0.22), value: vm.isConnected)
        }
        .windowStyle(.hiddenTitleBar)
        .commands {
            CommandMenu("Session") {
                Button(vm.isConnected ? "Disconnect" : "Connect") {
                    vm.connectOrDisconnect()
                }
                .keyboardShortcut("k", modifiers: [.command, .shift])

                Button("Refresh Online Hosts") {
                    vm.refreshHosts()
                }
                .keyboardShortcut("r", modifiers: .command)

                Divider()

                Button("Push Mac Clipboard to Windows") {
                    vm.syncClipboardToWindows()
                }
                .disabled(!vm.isConnected)

                Button("Pull Windows Clipboard to Mac") {
                    vm.pullClipboardFromWindows()
                }
                .disabled(!vm.isConnected)

                Button("Switch Windows Monitor") {
                    vm.cycleMonitor()
                }
                .disabled(!vm.isConnected)

                Button("Send Ctrl+Alt+Del") {
                    vm.bridge.sendCtrlAltDel()
                }
                .disabled(!vm.isConnected)

                Divider()

                ForEach(HeliosQualityMode.allCases) { mode in
                    Button(mode.fullLabel) {
                        vm.selectedMode = mode
                    }
                    .keyboardShortcut(KeyEquivalent(Character("\(mode.rawValue + 1)")), modifiers: .command)
                }
            }
        }
    }
}
