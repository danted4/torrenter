import SwiftUI
import UIKit
import UniformTypeIdentifiers

/// `.torrent`, as declared in Info.plist (falls back to the extension).
let torrentType = UTType("org.bittorrent.torrent")
    ?? UTType(filenameExtension: "torrent", conformingTo: .data)
    ?? .data

struct ContentView: View {
    @EnvironmentObject private var owner: EngineOwner
    @Environment(\.scenePhase) private var scenePhase
    @State private var themeId = Prefs.theme()
    @State private var input = ""
    @State private var magnet = true
    @State private var importing = false
    @State private var shareJob: JobView?
    @State private var settingsOpen = false
    @State private var settingsTab: SettingsTab = .appearance

    var colors: Palette { themePalette(themeId) }

    var body: some View {
        // The header stays on both screens: its cog turns into a back arrow
        // while settings fill the rest, and the title goes home.
        VStack(alignment: .leading, spacing: 12) {
            header(colors)
            if settingsOpen {
                SettingsView(themeId: $themeId, tab: $settingsTab)
            } else if owner.filesOpen != nil {
                FilesView(list: owner.fileList, owner: owner, colors: colors)
            } else {
                main
            }
        }
        .padding(16)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(colors.bg.ignoresSafeArea())
        .preferredColorScheme(.dark)
        .onAppear { owner.open() }
        .onChange(of: scenePhase) { phase in
            if phase == .background {
                owner.leaveForeground()
            } else if phase == .active {
                owner.enterForeground()
            }
        }
        .alert(
            owner.torrentTitle(owner.torrentAsk),
            isPresented: Binding(
                get: { owner.torrentAsk != nil },
                set: { if !$0 { owner.torrentAsk = nil } }
            ),
            presenting: owner.torrentAsk
        ) { ask in
            Button(owner.asksForPeers(ask) ? "download from peers" : "start") { owner.confirmTorrent(ask) }
            Button("cancel", role: .cancel) { owner.torrentAsk = nil }
        } message: { ask in
            Text(owner.torrentMessage(ask))
        }
        .fileImporter(isPresented: $importing, allowedContentTypes: [torrentType]) { result in
            if case .success(let url) = result {
                owner.queueTorrentFile(at: url)
            }
        }
        .sheet(item: shareSheetItem) { item in
            ShareSheet(items: owner.shareItems(for: item.job))
        }
        .confirmationDialog(
            "Also remove it from \(owner.remoteAsk.map { owner.accountTitle($0) } ?? "the service")?",
            isPresented: remoteAskShown,
            titleVisibility: .visible
        ) {
            // Clear or "as torrent" on a debrid job torrenter added.
            if let job = owner.remoteAsk {
                let rerouting = owner.clearAsk == nil
                let title = owner.accountTitle(job)
                Button("remove from \(title)", role: .destructive) {
                    owner.remoteAnswered(job: job, rerouting: rerouting, removeRemote: true)
                }
                Button("keep it on \(title)") {
                    owner.remoteAnswered(job: job, rerouting: rerouting, removeRemote: false)
                }
            }
        }
    }

    private var remoteAskShown: Binding<Bool> {
        Binding(
            get: { owner.remoteAsk != nil },
            set: { if !$0 { owner.remoteDismissed() } }
        )
    }

    private var main: some View {
        VStack(alignment: .leading, spacing: 12) {
            if let notice = owner.notice {
                Text(notice)
                    .font(.system(.footnote, design: .monospaced))
                    .foregroundColor(colors.fail)
            }
            modeSwitch(colors)
            urlField(colors)
            HStack(spacing: 12) {
                if magnet {
                    // Its own equatable view: the 200 ms job refresh must not
                    // rebuild the menu mid-tap (flicker, missed taps).
                    RouteMenu(
                        current: owner.account(owner.debridRoute),
                        accounts: owner.accounts,
                        colors: colors,
                        onPick: { owner.debridRoute = $0 },
                        onManage: {
                            settingsTab = .accounts
                            settingsOpen = true
                        }
                    )
                    .equatable()
                }
                Button("START") {
                    let source = input.trimmingCharacters(in: .whitespacesAndNewlines)
                    if owner.queue(kind: magnet ? .magnet : .http, source: source) {
                        input = ""
                    }
                }
                .buttonStyle(MonoButton(palette: colors))
            }
            HStack(spacing: 8) {
                Text("On My iPhone/torrenter")
                    .font(.system(size: 12, design: .monospaced))
                    .foregroundColor(colors.dim)
                Spacer(minLength: 8)
                Toggle(isOn: $owner.keepAliveOn) {
                    Text("keep alive")
                        .font(.system(size: 12, design: .monospaced))
                        .foregroundColor(owner.keepAliveOn ? colors.fg : colors.dim)
                }
                .toggleStyle(MiniSwitch(palette: colors))
                .fixedSize()
            }
            JobsSection(
                list: owner.jobList,
                owner: owner,
                names: owner.accountNames,
                colors: colors,
                shareJob: $shareJob
            )
            Spacer(minLength: 0)
        }
    }

    private var shareSheetItem: Binding<ShareIdent?> {
        Binding(
            get: { shareJob.map { ShareIdent(job: $0) } },
            set: { shareJob = $0?.job }
        )
    }

    private func header(_ colors: Palette) -> some View {
        HStack(alignment: .center, spacing: 10) {
            // The title doubles as "home": back to the main screen.
            Button {
                settingsOpen = false
                owner.filesOpen = nil
            } label: {
                HStack(spacing: 10) {
                    Image("Brand")
                        .resizable()
                        .frame(width: 28, height: 28)
                    Text(owner.bannerText.isEmpty ? "torrenter" : owner.bannerText)
                        .font(.system(.title3, design: .monospaced))
                        .foregroundColor(colors.fg)
                        .multilineTextAlignment(.leading)
                        .lineLimit(2)
                        .fixedSize(horizontal: false, vertical: true)
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityLabel("main screen")
            let inner = settingsOpen || owner.filesOpen != nil
            Button {
                if owner.filesOpen != nil {
                    owner.filesOpen = nil
                } else {
                    settingsOpen.toggle()
                }
            } label: {
                Image(systemName: inner ? "arrow.left" : "gearshape")
                    .font(.system(size: 20))
                    .foregroundColor(inner ? colors.fg : colors.dim)
                    .frame(width: 32, height: 32)
            }
            .accessibilityLabel(inner ? "back" : "settings")
        }
    }

    private func modeSwitch(_ colors: Palette) -> some View {
        HStack(spacing: 0) {
            modeLabel("MAGNET", on: magnet, colors: colors) { magnet = true }
            Text(" ⇄ ")
                .font(.system(.body, design: .monospaced))
                .foregroundColor(colors.dim)
            modeLabel("HTTP", on: !magnet, colors: colors) { magnet = false }
        }
    }

    private func modeLabel(_ title: String, on: Bool, colors: Palette, action: @escaping () -> Void) -> some View {
        Button(title, action: action)
            .font(.system(.body, design: .monospaced).weight(on ? .bold : .regular))
            .foregroundColor(on ? colors.fg : colors.dim)
    }

    private func urlField(_ colors: Palette) -> some View {
        TextField(
            "",
            text: $input,
            prompt: Text(magnet ? "magnet:?xt=urn:btih:…" : "https://example.com/file.iso")
                .foregroundColor(colors.dim)
        )
        .font(.system(size: 14, design: .monospaced))
        .foregroundColor(colors.fg)
        .padding(10)
        // Room for the upload button so text never runs under it.
        .padding(.trailing, magnet ? 30 : 0)
        .background(colors.row)
        .overlay(RoundedRectangle(cornerRadius: 2).stroke(colors.dim, lineWidth: 1))
        .overlay(alignment: .trailing) {
            if magnet {
                // Open a .torrent file; it starts at once, like START.
                Button {
                    importing = true
                } label: {
                    Image(systemName: "doc")
                        .font(.system(size: 16))
                        .foregroundColor(colors.dim)
                        .frame(width: 40, height: 40)
                }
                .accessibilityLabel("open a .torrent file")
            }
        }
        .textInputAutocapitalization(.never)
        .autocorrectionDisabled()
    }
}

/// Fixed-width route dropdown: "default (none)" (peers) or an account by
/// name, shortened with "…"; the menu lists each account with its provider
/// and ends with "manage accounts…". Equatable on what it shows, so job
/// updates do not redraw it.
private struct RouteMenu: View, Equatable {
    let current: DebridAccount?
    let accounts: [DebridAccount]
    let colors: Palette
    let onPick: (String?) -> Void
    let onManage: () -> Void

    /// Room for "default (none)" plus the chevron.
    static let width: CGFloat = 170

    static func == (lhs: RouteMenu, rhs: RouteMenu) -> Bool {
        lhs.current == rhs.current && lhs.accounts == rhs.accounts && lhs.colors == rhs.colors
    }

    var body: some View {
        let on = current != nil
        Menu {
            Button {
                onPick(nil)
            } label: {
                if on {
                    Text("default (none)")
                } else {
                    Label("default (none)", systemImage: "checkmark")
                }
                Text("from peers")
            }
            ForEach(accounts) { account in
                Button {
                    onPick(account.id)
                } label: {
                    if current?.id == account.id {
                        Label(account.name, systemImage: "checkmark")
                    } else {
                        Text(account.name)
                    }
                    Text(serviceLabel(account.service))
                }
            }
            Divider()
            Button(accounts.isEmpty ? "add a debrid account…" : "manage accounts…", action: onManage)
        } label: {
            ZStack {
                Text(current?.name ?? "default (none)")
                    .lineLimit(1)
                    .truncationMode(.tail)
                    .padding(.horizontal, 20)
                HStack {
                    Spacer()
                    Image(systemName: "chevron.down")
                        .font(.system(size: 10, weight: .semibold))
                }
                .padding(.trailing, 10)
            }
            .font(.system(size: 13, design: .monospaced))
            .foregroundColor(on ? colors.fg : colors.dim)
            .frame(width: Self.width, height: 34)
            .overlay(RoundedRectangle(cornerRadius: 2).stroke(on ? colors.fg : colors.dim, lineWidth: 1))
            .contentShape(Rectangle())
        }
    }
}

/// The job rows. Observes only the job list, so the 200 ms refresh redraws
/// just this section, not the URL field, dropdowns or settings.
private struct JobsSection: View {
    @ObservedObject var list: JobList
    /// Actions only; deliberately not observed here.
    let owner: EngineOwner
    let names: [String: String]
    let colors: Palette
    @Binding var shareJob: JobView?

    var body: some View {
        if list.jobs.isEmpty {
            Text("no jobs yet — paste a magnet or HTTP URL")
                .font(.system(.body, design: .monospaced))
                .foregroundColor(colors.dim)
        } else {
            ScrollView {
                LazyVStack(spacing: 10) {
                    ForEach(list.jobs, id: \.id) { job in
                        JobCard(
                            job: job,
                            names: names,
                            palette: colors,
                            onPause: { owner.togglePause(id: job.id) },
                            onStop: { owner.stop(id: job.id) },
                            onClear: { owner.clear(job: job) },
                            onRestart: { owner.restart(job: job) },
                            onShare: { shareJob = job },
                            onAsTorrent: { owner.asTorrent(job: job) },
                            onFiles: { owner.openFiles(job: job) }
                        )
                    }
                }
            }
        }
    }
}

private struct ShareIdent: Identifiable {
    let job: JobView
    var id: UInt64 { job.id }
}

private struct JobCard: View {
    let job: JobView
    let names: [String: String]
    let palette: Palette
    let onPause: () -> Void
    let onStop: () -> Void
    let onClear: () -> Void
    let onRestart: () -> Void
    let onShare: () -> Void
    let onAsTorrent: () -> Void
    let onFiles: () -> Void

    var body: some View {
        let fraction = jobBarFraction(job)
        let accent = job.active ? mix(palette.fg, palette.dim, 0.65) : palette.fg
        let lineColor = job.error != nil ? palette.fail : palette.dim
        VStack(alignment: .leading, spacing: 6) {
            if job.service != nil || job.kind == .http {
                Text(jobBadge(job, names: names))
                    .font(.system(size: 11, design: .monospaced))
                    .foregroundColor(palette.dim)
            }
            Text(job.title.isEmpty ? job.source : job.title)
                .font(.system(size: 13, design: .monospaced))
                .foregroundColor(accent)
                .lineLimit(2)
            if let fraction {
                ProgressView(value: Double(fraction))
                    .tint(accent)
            } else {
                ProgressView()
                    .tint(accent)
            }
            Text(jobStatsLine(job, names: names))
                .font(.system(size: 12, design: .monospaced))
                .foregroundColor(lineColor)
            HStack(spacing: 8) {
                if job.active {
                    Button(job.paused ? "resume" : "pause", action: onPause)
                        .buttonStyle(MonoButton(palette: palette))
                    Button("stop", action: onStop)
                        .buttonStyle(MonoButton(palette: palette))
                    if jobIsFetching(job) {
                        Button("as torrent", action: onAsTorrent)
                            .buttonStyle(MonoButton(palette: palette))
                    }
                } else {
                    Button("restart", action: onRestart)
                        .buttonStyle(MonoButton(palette: palette))
                    Button("clear", action: onClear)
                        .buttonStyle(MonoButton(palette: palette))
                    Button("share", action: onShare)
                        .buttonStyle(MonoButton(palette: palette))
                }
                if job.filesTotal >= 2 {
                    Button("files", action: onFiles)
                        .buttonStyle(MonoButton(palette: palette))
                }
            }
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(RoundedRectangle(cornerRadius: 4).fill(palette.row))
    }
}

/// Compact switch sized to the 12 pt monospaced labels (the system switch is too tall).
struct MiniSwitch: ToggleStyle {
    let palette: Palette

    func makeBody(configuration: Configuration) -> some View {
        Button {
            configuration.isOn.toggle()
        } label: {
            HStack(spacing: 6) {
                configuration.label
                ZStack(alignment: configuration.isOn ? .trailing : .leading) {
                    Capsule()
                        .fill(configuration.isOn ? palette.fg : palette.rowAlt)
                        .overlay(Capsule().stroke(palette.dim, lineWidth: configuration.isOn ? 0 : 1))
                        .frame(width: 26, height: 14)
                    Circle()
                        .fill(configuration.isOn ? palette.bg : palette.dim)
                        .frame(width: 10, height: 10)
                        .padding(.horizontal, 2)
                }
                .animation(.easeOut(duration: 0.12), value: configuration.isOn)
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityValue(configuration.isOn ? "on" : "off")
    }
}

struct MonoButton: ButtonStyle {
    let palette: Palette
    /// Text size; nil keeps the body size.
    var size: CGFloat?
    /// Text colour (remove / cancel use `palette.fail`).
    var tint: Color?
    /// A bright border, for the primary action once it can run.
    var outlined = false

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(size.map { .system(size: $0, design: .monospaced) } ?? .system(.body, design: .monospaced))
            .foregroundColor(tint ?? palette.fg)
            .padding(.horizontal, size == nil ? 12 : 10)
            .padding(.vertical, size == nil ? 8 : 6)
            .background(palette.rowAlt)
            .overlay(
                RoundedRectangle(cornerRadius: 2)
                    .stroke(outlined ? (tint ?? palette.fg) : Color.clear, lineWidth: 1)
            )
            .clipShape(RoundedRectangle(cornerRadius: 2))
            .opacity(configuration.isPressed ? 0.7 : 1)
    }
}

func mix(_ a: Color, _ b: Color, _ towardB: Double) -> Color {
    let t = min(max(towardB, 0), 1)
    let aa = UIColor(a)
    let bb = UIColor(b)
    var ar: CGFloat = 0, ag: CGFloat = 0, ab: CGFloat = 0, ao: CGFloat = 0
    var br: CGFloat = 0, bg: CGFloat = 0, bb2: CGFloat = 0, bo: CGFloat = 0
    aa.getRed(&ar, green: &ag, blue: &ab, alpha: &ao)
    bb.getRed(&br, green: &bg, blue: &bb2, alpha: &bo)
    return Color(
        red: ar * (1 - t) + br * t,
        green: ag * (1 - t) + bg * t,
        blue: ab * (1 - t) + bb2 * t
    )
}
