// HiSettings — the native macOS Settings window, a SwiftUI client of the engine's
// local config API (docs/core-shell-config-api.md). This is Phase 1 of the UI-arch
// refactor: the SwiftUI window replaces the hand-laid objc2 preferences window while
// the Rust process still owns the app. It reads/writes settings over HTTP (never via
// FFI into engine state); the only FFI is the single C entry point `hi_settings_open`.
//
// Build: compiled by build.rs into a static lib and linked on macOS only (see build.rs).
// The Rust side calls `hi_settings_open(port)` from the tray's "Settings…" action.
//
// NOTE (unbuilt): written without a macOS toolchain to compile against — expect to
// build + fix-forward on a Mac. The likely hotspots are the Swift-runtime link in
// build.rs and any availability gaps below.

import AppKit
import SwiftUI

// MARK: - C entry point

/// Open (or focus) the Settings window. Called from Rust on the main thread; we still
/// hop to main defensively since AppKit/SwiftUI are main-thread only. `port` is the
/// local HTTP server's port, used to build the API base URL.
@_cdecl("hi_settings_open")
public func hi_settings_open(_ port: UInt16) {
    DispatchQueue.main.async {
        SettingsWindowController.shared.show(port: port)
    }
}

// MARK: - Window

/// Owns the single reused Settings window (a preferences window is a singleton — a
/// second "Settings…" just brings the first forward).
final class SettingsWindowController {
    static let shared = SettingsWindowController()
    private var window: NSWindow?

    func show(port: UInt16) {
        NSApp.setActivationPolicy(.regular)
        NSApp.activate(ignoringOtherApps: true)

        if let window = window {
            window.makeKeyAndOrderFront(nil)
            return
        }

        let root = SettingsRootView(api: SettingsAPI(port: port))
        let hosting = NSHostingController(rootView: root)
        let window = NSWindow(contentViewController: hosting)
        window.title = "Settings"
        window.styleMask = [.titled, .closable, .miniaturizable]
        window.setContentSize(NSSize(width: 780, height: 520))
        window.isReleasedWhenClosed = false
        window.center()
        window.makeKeyAndOrderFront(nil)
        self.window = window
    }
}

// MARK: - API client

/// Thin async client over the engine's config API. Mirrors the serde DTOs; the read
/// surface never carries an api_key.
struct SettingsAPI {
    let port: UInt16
    private var base: String { "http://127.0.0.1:\(port)" }

    private static let decoder: JSONDecoder = {
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        return d
    }()
    private static let encoder: JSONEncoder = {
        let e = JSONEncoder()
        e.keyEncodingStrategy = .convertToSnakeCase
        return e
    }()

    func get() async throws -> SettingsSnapshot {
        try await request("/api/settings", method: "GET", body: Optional<Empty>.none)
    }

    func putAppearance(_ patch: AppearancePatch) async throws -> AppearanceState {
        try await request("/api/settings/appearance", method: "PUT", body: patch)
    }

    func putMode(_ mode: String) async throws {
        let _: ModeEcho = try await request("/api/settings/mode", method: "PUT", body: ModePatch(mode: mode))
    }

    func putFeature(_ feature: String, _ patch: FeaturePatch) async throws -> FeatureStatus {
        try await request("/api/settings/credentials/\(feature)", method: "PUT", body: patch)
    }

    func refreshEnergy() async throws -> EnergyEcho {
        try await request("/api/account/energy/refresh", method: "POST", body: Optional<Empty>.none)
    }

    /// Fetch a signed-in "manage account" URL (falls back to the plain account page).
    func subscribeURL() async -> URL {
        let fallback = URL(string: "https://hi.xiaoyuanzhu.com/account")!
        struct Sub: Decodable { let url: String }
        guard let sub: Sub = try? await request("/api/account/subscribe", method: "GET", body: Optional<Empty>.none),
              let u = URL(string: sub.url) else { return fallback }
        return u
    }

    func signInURL() -> URL { URL(string: "\(base)/account/link/start")! }

    private func request<B: Encodable, T: Decodable>(_ path: String, method: String, body: B?) async throws -> T {
        var req = URLRequest(url: URL(string: base + path)!)
        req.httpMethod = method
        if let body = body {
            req.setValue("application/json", forHTTPHeaderField: "content-type")
            req.httpBody = try Self.encoder.encode(body)
        }
        let (data, resp) = try await URLSession.shared.data(for: req)
        guard let http = resp as? HTTPURLResponse, (200..<300).contains(http.statusCode) else {
            throw APIError.status((resp as? HTTPURLResponse)?.statusCode ?? -1)
        }
        return try Self.decoder.decode(T.self, from: data)
    }
}

enum APIError: Error { case status(Int) }
struct Empty: Codable {}

// MARK: - DTOs (mirror src/foundation/server/settings.rs)

struct SettingsSnapshot: Decodable {
    var appearance: AppearanceState
    var account: AccountState
    var about: AboutState
}
struct AppearanceState: Decodable {
    var theme: ChoiceSetting
    var language: ChoiceSetting
    var gestures: FlagSetting
}
struct ChoiceSetting: Decodable { var value: String; var options: [Choice]; var applies: String }
struct Choice: Decodable, Hashable { var value: String; var label: String }
struct FlagSetting: Decodable { var value: Bool; var applies: String }
struct AccountState: Decodable {
    var mode: String
    var identity: IdentityState
    var energy: EnergySnapshot?
    var features: [FeatureStatus]
}
struct IdentityState: Decodable { var signedIn: Bool; var name: String?; var email: String? }
struct EnergySnapshot: Decodable { var tier: String; var remaining: Int; var total: Int; var resetsAt: String; var outOfEnergy: Bool }
struct FeatureStatus: Decodable, Identifiable { var feature: String; var configured: Bool; var baseUrl: String?; var model: String?; var id: String { feature } }
struct AboutState: Decodable { var version: String; var website: String }

struct AppearancePatch: Encodable { var theme: String?; var language: String?; var gestures: Bool? }
struct ModePatch: Encodable { var mode: String }
struct ModeEcho: Decodable { var mode: String }
struct FeaturePatch: Encodable { var apiKey: String?; var baseUrl: String?; var model: String? }
struct EnergyEcho: Decodable { var energy: EnergySnapshot? }

let featureLabels: [String: String] = [
    "llm": "Language model", "stt": "Speech-to-text", "tts": "Text-to-speech",
    "vision": "Vision", "image": "Image", "video": "Video",
]

// MARK: - View model

@MainActor
final class SettingsModel: ObservableObject {
    @Published var snap: SettingsSnapshot?
    @Published var error: String?
    @Published var restartHint = false

    let api: SettingsAPI
    init(api: SettingsAPI) { self.api = api }

    func load() async {
        do {
            let s = try await api.get()
            snap = s
            applyTheme(s.appearance.theme.value)
        } catch { self.error = "\(error)" }
    }

    func setTheme(_ value: String) async {
        await patchAppearance(AppearancePatch(theme: value, language: nil, gestures: nil), restart: false)
        applyTheme(value)
    }
    func setLanguage(_ value: String) async {
        await patchAppearance(AppearancePatch(theme: nil, language: value, gestures: nil), restart: true)
    }
    func setGestures(_ on: Bool) async {
        await patchAppearance(AppearancePatch(theme: nil, language: nil, gestures: on), restart: true)
    }

    private func patchAppearance(_ patch: AppearancePatch, restart: Bool) async {
        do {
            let next = try await api.putAppearance(patch)
            snap?.appearance = next
            if restart { restartHint = true }
        } catch { self.error = "\(error)" }
    }

    func setMode(_ mode: String) async {
        do {
            try await api.putMode(mode)
            snap?.account.mode = mode
        } catch { self.error = "\(error)" }
    }

    func saveFeature(_ feature: String, _ patch: FeaturePatch) async {
        do {
            let next = try await api.putFeature(feature, patch)
            if let i = snap?.account.features.firstIndex(where: { $0.feature == feature }) {
                snap?.account.features[i] = next
            }
        } catch { self.error = "\(error)" }
    }

    func refreshEnergy() async {
        do { snap?.account.energy = try await api.refreshEnergy().energy }
        catch { self.error = "\(error)" }
    }

    /// Live theme apply: force NSApp.appearance (drives native chrome and the WKWebView
    /// face together); "system" clears it to follow the OS. This is the shell side of
    /// "core persists, shell applies".
    private func applyTheme(_ value: String) {
        switch value {
        case "light": NSApp.appearance = NSAppearance(named: .aqua)
        case "dark": NSApp.appearance = NSAppearance(named: .darkAqua)
        default: NSApp.appearance = nil
        }
    }
}

// MARK: - Views

enum SettingsPane: String, CaseIterable, Identifiable {
    case general
    case account
    case about

    var id: String { rawValue }

    var title: String {
        switch self {
        case .general: return "General"
        case .account: return "Account"
        case .about: return "About"
        }
    }

    var symbol: String {
        switch self {
        case .general: return "gearshape"
        case .account: return "person.crop.circle"
        case .about: return "info.circle"
        }
    }
}

struct SettingsRootView: View {
    @StateObject private var model: SettingsModel
    @State private var selectedPane: SettingsPane = .general
    init(api: SettingsAPI) { _model = StateObject(wrappedValue: SettingsModel(api: api)) }

    var body: some View {
        Group {
            if let snap = model.snap {
                HStack(spacing: 0) {
                    SettingsSidebar(selection: $selectedPane)
                    Divider()
                    SettingsDetailPane(title: selectedPane.title) {
                        switch selectedPane {
                        case .general:
                            GeneralTab(model: model, appearance: snap.appearance)
                        case .account:
                            AccountTab(model: model, account: snap.account)
                        case .about:
                            AboutTab(about: snap.about)
                        }
                    }
                }
                .frame(width: 780, height: 520)
                .background(Color(NSColor.windowBackgroundColor))
            } else if let error = model.error {
                Text("Couldn’t load settings: \(error)").padding()
            } else {
                ProgressView().padding()
            }
        }
        .task { await model.load() }
    }
}

struct SettingsSidebar: View {
    @Binding var selection: SettingsPane

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Settings")
                .font(.title3)
                .fontWeight(.semibold)
                .padding(.horizontal, 14)
                .padding(.top, 18)
                .padding(.bottom, 8)

            ForEach(SettingsPane.allCases) { pane in
                Button {
                    selection = pane
                } label: {
                    HStack(spacing: 10) {
                        Image(systemName: pane.symbol)
                            .frame(width: 18)
                        Text(pane.title)
                            .lineLimit(1)
                        Spacer()
                    }
                    .padding(.horizontal, 12)
                    .padding(.vertical, 7)
                    .contentShape(Rectangle())
                    .background(
                        RoundedRectangle(cornerRadius: 7)
                            .fill(selection == pane ? Color.accentColor.opacity(0.18) : Color.clear)
                    )
                }
                .buttonStyle(.plain)
                .foregroundColor(selection == pane ? .primary : .secondary)
                .padding(.horizontal, 8)
            }

            Spacer()
        }
        .frame(width: 190)
        .background(Color(NSColor.controlBackgroundColor))
    }
}

struct SettingsDetailPane<Content: View>: View {
    let title: String
    let content: Content

    init(title: String, @ViewBuilder content: () -> Content) {
        self.title = title
        self.content = content()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text(title)
                .font(.title2)
                .fontWeight(.semibold)
                .padding(.horizontal, 28)
                .padding(.top, 24)
                .padding(.bottom, 16)
            Divider()
            content
                .padding(28)
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

struct GeneralTab: View {
    @ObservedObject var model: SettingsModel
    let appearance: AppearanceState

    var body: some View {
        Form {
            Picker("Theme", selection: Binding(
                get: { appearance.theme.value },
                set: { v in Task { await model.setTheme(v) } })) {
                ForEach(appearance.theme.options, id: \.value) { Text($0.label).tag($0.value) }
            }
            Picker("Language", selection: Binding(
                get: { appearance.language.value },
                set: { v in Task { await model.setLanguage(v) } })) {
                ForEach(appearance.language.options, id: \.value) { Text($0.label).tag($0.value) }
            }
            Toggle("Attention gestures", isOn: Binding(
                get: { appearance.gestures.value },
                set: { v in Task { await model.setGestures(v) } }))
            if model.restartHint {
                Text("Some changes take effect the next time Hi Agent starts.")
                    .font(.footnote).foregroundColor(.secondary)
            }
        }
    }
}

struct AccountTab: View {
    @ObservedObject var model: SettingsModel
    let account: AccountState

    var body: some View {
        Form {
            Picker("Credentials", selection: Binding(
                get: { account.mode },
                set: { v in Task { await model.setMode(v) } })) {
                Text("小圆猪 (managed)").tag("xiaoyuanzhu")
                Text("Your own keys").tag("byok")
            }
            .pickerStyle(.segmented)

            if account.mode == "xiaoyuanzhu" {
                ManagedSection(model: model, account: account)
            } else {
                ForEach(account.features) { f in
                    FeatureRow(model: model, status: f)
                }
            }
        }
    }
}

struct ManagedSection: View {
    @ObservedObject var model: SettingsModel
    let account: AccountState
    @State private var subscribeURL: URL?

    var body: some View {
        Group {
            LabelRow("Signed in") {
                Text(account.identity.signedIn
                     ? "as \(account.identity.name ?? account.identity.email ?? "your account")"
                     : "Not signed in")
                    .foregroundColor(.secondary)
            }
            LabelRow("Plan") {
                Text(account.energy.map { $0.tier == "sub" ? "Subscribed" : "Free" } ?? "—")
            }
            LabelRow("Energy") {
                HStack {
                    Text(account.energy.map { "\($0.remaining) / \($0.total)" } ?? "—")
                    Button("Refresh") { Task { await model.refreshEnergy() } }.buttonStyle(.link)
                }
            }
            HStack {
                if !account.identity.signedIn {
                    Button("Sign in") { NSWorkspace.shared.open(model.api.signInURL()) }
                }
                Button("Manage account") {
                    if let u = subscribeURL { NSWorkspace.shared.open(u) }
                }
            }
        }
        .task { subscribeURL = await model.api.subscribeURL() }
    }
}

/// A label/value row (avoids `LabeledContent`, which is macOS 13+).
struct LabelRow<Content: View>: View {
    let label: String
    let content: Content
    init(_ label: String, @ViewBuilder content: () -> Content) {
        self.label = label
        self.content = content()
    }
    var body: some View {
        HStack {
            Text(label).foregroundColor(.secondary)
            Spacer()
            content
        }
    }
}

struct FeatureRow: View {
    @ObservedObject var model: SettingsModel
    let status: FeatureStatus
    @State private var editing = false

    var body: some View {
        HStack {
            Text(featureLabels[status.feature] ?? status.feature)
            Spacer()
            Text(status.configured ? "configured" : "not set")
                .font(.caption).foregroundColor(status.configured ? .green : .secondary)
            Button("Edit…") { editing = true }.buttonStyle(.link)
        }
        .sheet(isPresented: $editing) {
            FeatureEditor(model: model, status: status, isPresented: $editing)
        }
    }
}

struct FeatureEditor: View {
    @ObservedObject var model: SettingsModel
    let status: FeatureStatus
    @Binding var isPresented: Bool

    @State private var apiKey = ""
    @State private var baseUrl = ""
    @State private var modelName = ""
    @State private var saving = false

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(featureLabels[status.feature] ?? status.feature).font(.headline)
            SecureField(status.configured ? "•••••• (leave blank to keep)" : "API key", text: $apiKey)
            TextField("Base URL (optional)", text: $baseUrl)
            TextField("Model (optional)", text: $modelName)
            HStack {
                Spacer()
                Button("Cancel") { isPresented = false }
                Button(saving ? "Saving…" : "Save") {
                    saving = true
                    Task {
                        // Blank api_key → nil so the engine keeps the stored key.
                        let key = apiKey.trimmingCharacters(in: .whitespaces)
                        await model.saveFeature(status.feature, FeaturePatch(
                            apiKey: key.isEmpty ? nil : key,
                            baseUrl: baseUrl, model: modelName))
                        saving = false
                        isPresented = false
                    }
                }
                .keyboardShortcut(.defaultAction)
                .disabled(saving)
            }
        }
        .padding(20)
        .frame(width: 380)
        .onAppear {
            baseUrl = status.baseUrl ?? ""
            modelName = status.model ?? ""
        }
    }
}

struct AboutTab: View {
    let about: AboutState
    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Hi Agent").font(.title2).bold()
            Text("Version \(about.version)").foregroundColor(.secondary)
            Link(about.website.replacingOccurrences(of: "https://", with: ""),
                 destination: URL(string: about.website)!)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}
