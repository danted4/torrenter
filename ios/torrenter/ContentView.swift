import SwiftUI
import UIKit

struct ContentView: View {
    @EnvironmentObject private var owner: EngineOwner
    @Environment(\.scenePhase) private var scenePhase
    @State private var themeId = Prefs.theme()
    @State private var input = ""
    @State private var magnet = true
    @State private var shareJob: JobView?
    @State private var settingsOpen = false

    var colors: Palette { themePalette(themeId) }

    var body: some View {
        Group {
            if settingsOpen {
                SettingsView(themeId: $themeId) { settingsOpen = false }
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
        .sheet(item: shareSheetItem) { item in
            ShareSheet(items: owner.shareItems(for: item.job))
        }
        .confirmationDialog(
            "Also remove it from \(serviceLabel(owner.clearAsk?.service))?",
            isPresented: clearAskShown,
            titleVisibility: .visible
        ) {
            if let job = owner.clearAsk {
                Button("remove from \(serviceLabel(job.service))", role: .destructive) {
                    owner.clearConfirmed(job: job, removeRemote: true)
                }
                Button("keep it on \(serviceLabel(job.service))") {
                    owner.clearConfirmed(job: job, removeRemote: false)
                }
            }
        }
    }

    private var clearAskShown: Binding<Bool> {
        Binding(
            get: { owner.clearAsk != nil },
            set: { if !$0 { owner.clearAsk = nil } }
        )
    }

    private var main: some View {
        VStack(alignment: .leading, spacing: 12) {
            header(colors)
            if let notice = owner.notice {
                Text(notice)
                    .font(.system(.footnote, design: .monospaced))
                    .foregroundColor(colors.fail)
            }
            modeSwitch(colors)
            urlField(colors)
            HStack(spacing: 12) {
                Button("START") {
                    let source = input.trimmingCharacters(in: .whitespacesAndNewlines)
                    if owner.queue(kind: magnet ? .magnet : .http, source: source) {
                        input = ""
                    }
                }
                .buttonStyle(MonoButton(palette: colors))
                if magnet {
                    debridControl(colors)
                }
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
            if owner.jobs.isEmpty {
                Text("no jobs yet — paste a magnet or HTTP URL")
                    .font(.system(.body, design: .monospaced))
                    .foregroundColor(colors.dim)
            } else {
                ScrollView {
                    LazyVStack(spacing: 10) {
                        ForEach(owner.jobs, id: \.id) { job in
                            JobCard(
                                job: job,
                                palette: colors,
                                onPause: { owner.togglePause(id: job.id) },
                                onStop: { owner.stop(id: job.id) },
                                onClear: { owner.clear(job: job) },
                                onRestart: { owner.restart(job: job) },
                                onShare: { shareJob = job },
                                onAsTorrent: { owner.asTorrent(job: job) }
                            )
                        }
                    }
                }
            }
            Spacer(minLength: 0)
        }
    }

    /// "debrid" (off) or "via TorBox"; tap cycles through services with a key.
    private func debridControl(_ colors: Palette) -> some View {
        let on = owner.debridRoute != nil
        return Button {
            owner.cycleDebridRoute()
            if owner.keyed.isEmpty {
                settingsOpen = true
            }
        } label: {
            Text(on ? "via \(serviceLabel(owner.debridRoute))" : "debrid")
                .font(.system(size: 13, design: .monospaced))
                .foregroundColor(on ? colors.fg : colors.dim)
                .padding(.horizontal, 10)
                .padding(.vertical, 6)
                .overlay(
                    RoundedRectangle(cornerRadius: 4).stroke(on ? colors.fg : colors.dim, lineWidth: 1)
                )
        }
        .buttonStyle(.plain)
    }

    private var shareSheetItem: Binding<ShareIdent?> {
        Binding(
            get: { shareJob.map { ShareIdent(job: $0) } },
            set: { shareJob = $0?.job }
        )
    }

    private func header(_ colors: Palette) -> some View {
        HStack(alignment: .center, spacing: 10) {
            Image("Brand")
                .resizable()
                .frame(width: 28, height: 28)
            Text(owner.bannerText.isEmpty ? "torrenter" : owner.bannerText)
                .font(.system(.title3, design: .monospaced))
                .foregroundColor(colors.fg)
                .multilineTextAlignment(.leading)
                .lineLimit(2)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
            Button {
                settingsOpen = true
            } label: {
                Image(systemName: "gearshape")
                    .font(.system(size: 20))
                    .foregroundColor(colors.dim)
            }
            .accessibilityLabel("settings")
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
        .background(colors.row)
        .overlay(RoundedRectangle(cornerRadius: 2).stroke(colors.dim, lineWidth: 1))
        .textInputAutocapitalization(.never)
        .autocorrectionDisabled()
    }
}

private struct ShareIdent: Identifiable {
    let job: JobView
    var id: UInt64 { job.id }
}

private struct JobCard: View {
    let job: JobView
    let palette: Palette
    let onPause: () -> Void
    let onStop: () -> Void
    let onClear: () -> Void
    let onRestart: () -> Void
    let onShare: () -> Void
    let onAsTorrent: () -> Void

    var body: some View {
        let fraction = jobBarFraction(job)
        let accent = job.active ? mix(palette.fg, palette.dim, 0.65) : palette.fg
        let lineColor = job.error != nil ? palette.fail : palette.dim
        VStack(alignment: .leading, spacing: 6) {
            if job.service != nil || job.kind == .http {
                Text(jobBadge(job))
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
            Text(jobStatsLine(job))
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

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.system(.body, design: .monospaced))
            .foregroundColor(palette.fg)
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
            .background(palette.rowAlt)
            .clipShape(RoundedRectangle(cornerRadius: 2))
            .opacity(configuration.isPressed ? 0.7 : 1)
    }
}

private func mix(_ a: Color, _ b: Color, _ towardB: Double) -> Color {
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
