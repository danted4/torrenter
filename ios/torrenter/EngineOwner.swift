import Combine
import Foundation
import SwiftUI
import UIKit

@MainActor
final class EngineOwner: ObservableObject {
    static let tickMs: UInt64 = 200

    /// The job list lives in its own object so only the job rows redraw on
    /// the 200 ms refresh; the rest of the screen (URL field, dropdowns,
    /// settings) does not.
    let jobList = JobList()
    var jobs: [JobView] { jobList.jobs }
    @Published var notice: String?
    @Published private(set) var bannerText: String = ""
    @Published var keepAliveOn: Bool = Prefs.keepAlive() {
        didSet {
            Prefs.saveKeepAlive(keepAliveOn)
            syncKeepAlive()
        }
    }
    /// Remembered debrid account id for magnet jobs (nil = default, peers).
    @Published var debridRoute: String? = Prefs.debridRoute() {
        didSet { Prefs.saveDebridRoute(debridRoute) }
    }
    /// Connections per download; 0 = Auto (4).
    @Published var connections: Int = Prefs.connections() {
        didSet {
            Prefs.saveConnections(connections)
            engine?.setConnections(parts: connections > 0 ? UInt8(min(connections, 16)) : nil)
        }
    }
    @Published var removeAfter: Bool = Prefs.removeAfter() {
        didSet {
            Prefs.saveRemoveAfter(removeAfter)
            engine?.setDebridRemoveAfter(enabled: removeAfter)
        }
    }
    /// Debrid accounts with a key in the Keychain, in the order added.
    @Published private(set) var accounts: [DebridAccount] = []
    /// Last account check per account id ("premium until …" or the error).
    @Published private(set) var accountNotes: [String: String] = [:]
    /// A cleared debrid job that may still be on the service: ask first.
    @Published var clearAsk: JobView?
    /// "as torrent" on a debrid job torrenter added: ask before it leaves the service.
    @Published var rerouteAsk: JobView?

    /// The job the "also remove it from …?" question is about, if any.
    var remoteAsk: JobView? { clearAsk ?? rerouteAsk }

    private var engine: Engine?
    private var ticker: Task<Void, Never>?
    private let keepAlive = KeepAlive()
    private var inBackground = false
    private var graceTask: Task<Void, Never>?
    private var graceId: UIBackgroundTaskIdentifier = .invalid
    private var keepAliveRetryAt = Date.distantPast
    private var keepAliveFailing = false
    /// Largest .torrent read (the engine refuses anything larger too).
    nonisolated static let maxTorrentFile = 32 * 1024 * 1024
    /// A .torrent from "Open with" (another app can send one), started only
    /// after a tap.
    @Published var torrentAsk: TorrentAsk?
    /// .torrent files opened before the engine was up ("Open with" at launch).
    private var pendingTorrents: [(data: Data, peersOnly: Bool)] = []

    static let keepAliveNotice = "keep alive could not start audio; jobs park when you leave"
    static let keepAliveRetry: TimeInterval = 10

    func open() {
        if engine != nil {
            return
        }
        bannerText = banner()
        let dest = Paths.publicDest()
        try? FileManager.default.createDirectory(at: dest, withIntermediateDirectories: true)
        let created = Engine.open(
            dest: dest.path,
            jobsPath: Paths.jobsFile().path,
            seed: false
        )
        // Keys go in before hydrate so restored debrid jobs can start.
        accounts = loadAccounts()
        for account in accounts {
            if let key = Keychain.read(account.id) {
                try? created.setDebridAccount(id: account.id, service: account.service, key: key)
            }
        }
        if let route = debridRoute, !accounts.contains(where: { $0.id == route }) {
            debridRoute = nil
        }
        created.setConnections(parts: connections > 0 ? UInt8(min(connections, 16)) : nil)
        created.setDebridRemoveAfter(enabled: removeAfter)
        do {
            try created.hydrate()
        } catch {
            notice = engineMessage(error)
        }
        engine = created
        keepAlive.onInterrupted = { [weak self] in
            guard let self, self.inBackground else { return }
            self.beginGrace()
        }
        keepAlive.onRestored = { [weak self] in
            guard let self, self.graceTask != nil else { return }
            self.graceTask?.cancel()
            self.graceTask = nil
            self.endGrace()
        }
        resumeParked()
        refresh()
        startTicker()
        let pending = pendingTorrents
        pendingTorrents = []
        pending.forEach { queueTorrentData($0.data, peersOnly: $0.peersOnly) }
    }

    /// A .torrent from the picker or "Open with": read (security-scoped,
    /// capped) off the main actor, so an iCloud file still downloading never
    /// blocks the UI, then queued through the current route, like START.
    /// The picker: the user chose it, so it starts at once.
    func queueTorrentFile(at url: URL) {
        takeTorrent(at: url, ask: false)
    }

    /// "Open with": shown with its name and route first (`torrentAsk`).
    func askTorrentFile(at url: URL) {
        takeTorrent(at: url, ask: true)
    }

    func confirmTorrent(_ ask: TorrentAsk) {
        torrentAsk = nil
        // A private torrent never goes through debrid: from its peers.
        queueTorrentData(ask.data, peersOnly: ask.isPrivate)
    }

    /// The confirm's second line: the route, or why a private torrent ignores it.
    func routeLine(_ ask: TorrentAsk) -> String {
        ask.isPrivate ? "private torrent: from its tracker's peers only" : "via \(routeName)"
    }

    /// Where a new magnet goes: the chosen debrid account's name, or peers.
    var routeName: String {
        accounts.first { $0.id == debridRoute }?.name ?? "peers"
    }

    private func takeTorrent(at url: URL, ask: Bool) {
        Task { [weak self] in
            let result = await Task.detached(priority: .userInitiated) {
                Self.readTorrent(at: url)
            }.value
            guard let self else { return }
            switch result {
            case .success(let data) where ask:
                do {
                    let view = try inspectTorrentFile(bytes: data)
                    self.torrentAsk = TorrentAsk(data: data, name: view.name, isPrivate: view.private)
                } catch {
                    self.notice = engineMessage(error)
                }
            case .success(let data): self.queueTorrentData(data)
            case .failure(let error): self.notice = error.message
            }
        }
    }

    private struct ReadFailure: Error, Sendable { let message: String }

    private nonisolated static func readTorrent(at url: URL) -> Result<Data, ReadFailure> {
        let scoped = url.startAccessingSecurityScopedResource()
        defer {
            if scoped { url.stopAccessingSecurityScopedResource() }
            // Only a copy iOS put in THIS app's Documents/Inbox (it would show
            // up in Files); never a user's file elsewhere.
            if isInOwnInbox(url) {
                try? FileManager.default.removeItem(at: url)
            }
        }
        let size = (try? url.resourceValues(forKeys: [.fileSizeKey]).fileSize) ?? 0
        if size > maxTorrentFile {
            return .failure(ReadFailure(message: "the .torrent file is larger than 32 MB"))
        }
        guard let data = try? Data(contentsOf: url) else {
            return .failure(ReadFailure(message: "could not read that file"))
        }
        if data.count > maxTorrentFile {
            return .failure(ReadFailure(message: "the .torrent file is larger than 32 MB"))
        }
        return .success(data)
    }

    /// Whether `url` lies inside this app's own Documents/Inbox (paths
    /// standardised and symlinks resolved before comparing).
    nonisolated static func isInOwnInbox(_ url: URL) -> Bool {
        guard let documents = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask).first else {
            return false
        }
        let inbox = documents.appendingPathComponent("Inbox", isDirectory: true)
            .standardizedFileURL.resolvingSymlinksInPath().path
        let file = url.standardizedFileURL.resolvingSymlinksInPath().path
        return file.hasPrefix(inbox + "/")
    }

    private func queueTorrentData(_ data: Data, peersOnly: Bool = false) {
        guard let engine else {
            pendingTorrents.append((data, peersOnly))
            return
        }
        do {
            _ = try engine.queueTorrentFile(bytes: data, account: peersOnly ? nil : debridRoute)
            notice = nil
            refresh()
        } catch {
            notice = engineMessage(error)
        }
    }

    @discardableResult
    func queue(kind: Kind, source: String) -> Bool {
        guard let engine else { return false }
        do {
            if kind == .magnet, let account = debridRoute {
                _ = try engine.queueDebrid(source: source, account: account)
            } else {
                _ = try engine.queue(kind: kind, source: source)
            }
            notice = nil
            refresh()
            return true
        } catch {
            notice = engineMessage(error)
            return false
        }
    }

    func togglePause(id: UInt64) {
        engine?.togglePause(id: id)
        refresh()
    }

    func stop(id: UInt64) {
        engine?.stop(id: id)
        refresh()
    }

    func remove(id: UInt64) {
        engine?.remove(id: id)
        refresh()
    }

    /// Clear button: ask first when the job is a debrid torrent we added that
    /// never finished (it may still sit on the service).
    func clear(job: JobView) {
        if engine?.clearAsksAboutRemote(id: job.id) == true {
            clearAsk = job
        } else {
            remove(id: job.id)
        }
    }

    func clearConfirmed(job: JobView, removeRemote: Bool) {
        if removeRemote {
            engine?.removeWithRemote(id: job.id)
        } else {
            engine?.remove(id: job.id)
        }
        clearAsk = nil
        refresh()
    }

    /// Answer to the remote question: finish the clear or the re-route. The
    /// dialog passes what it showed (its binding may already be cleared).
    func remoteAnswered(job: JobView, rerouting: Bool, removeRemote: Bool) {
        remoteDismissed()
        if rerouting {
            restartAsTorrent(job: job, removeRemote: removeRemote)
        } else {
            clearConfirmed(job: job, removeRemote: removeRemote)
        }
    }

    /// Dismissed without an answer: neither clear nor re-route.
    func remoteDismissed() {
        clearAsk = nil
        rerouteAsk = nil
    }

    /// Stop waiting for the service and download the same torrent from peers.
    func asTorrent(job: JobView) {
        if engine?.rerouteAsksAboutRemote(id: job.id, account: nil) == true {
            rerouteAsk = job
        } else {
            restartAsTorrent(job: job, removeRemote: false)
        }
    }

    private func restartAsTorrent(job: JobView, removeRemote: Bool) {
        guard let engine else { return }
        do {
            _ = try engine.restartRoute(
                id: job.id,
                account: nil,
                seed: false,
                dest: Paths.publicDest().path,
                removeFromService: removeRemote
            )
            notice = nil
            refresh()
        } catch {
            notice = engineMessage(error)
        }
    }

    /// Accounts this build can use. A key saved per provider before accounts
    /// existed becomes an account with that id. An entry leaves the stored list
    /// only when its Keychain item is provably gone; a read that fails for
    /// another reason (before first unlock) or an unknown provider never
    /// removes it, and the offered-provider filter applies in memory only.
    private func loadAccounts() -> [DebridAccount] {
        let stored = Self.storedAccounts()
        var list = stored
        for service in debridServices() where !list.contains(where: { $0.id == service.id }) {
            if Keychain.exists(service.id) {
                list.append(DebridAccount(id: service.id, name: service.label, service: service.id))
            }
        }
        list = list.filter { Keychain.exists($0.id) }
        if list != stored {
            Prefs.saveAccounts(list)
        }
        let offered = Set(debridServices().map(\.id))
        return list.filter { offered.contains($0.service) }
    }

    /// The saved list, one entry per id (a hand-edited list may repeat one).
    private static func storedAccounts() -> [DebridAccount] {
        var seen = Set<String>()
        return Prefs.accounts().filter { seen.insert($0.id).inserted }
    }

    func account(_ id: String?) -> DebridAccount? {
        guard let id else { return nil }
        return accounts.first { $0.id == id }
    }

    /// "work (TorBox)", or just "TorBox" when the name is the provider's.
    func accountTitle(_ job: JobView) -> String {
        guard let account = account(job.account) else { return serviceLabel(job.service) }
        let provider = serviceLabel(account.service)
        return account.name == provider ? provider : "\(account.name) (\(provider))"
    }

    /// Account names by id, for job rows.
    var accountNames: [String: String] {
        Dictionary(accounts.map { ($0.id, $0.name) }, uniquingKeysWith: { first, _ in first })
    }

    /// The name a new account gets when none is typed: the provider's, then
    /// "TorBox 2", …
    func defaultName(service: String) -> String {
        let base = serviceLabel(service)
        if !nameTaken(base) {
            return base
        }
        var n = 2
        while nameTaken("\(base) \(n)") {
            n += 1
        }
        return "\(base) \(n)"
    }

    /// Names are unique across providers, ignoring case.
    func nameTaken(_ name: String) -> Bool {
        let wanted = name.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        return accounts.contains { $0.name.lowercased() == wanted }
    }

    /// Save a new account: the key to the Keychain, the name to settings.
    /// Returns false (with a notice) when it could not be saved.
    @discardableResult
    func addAccount(service: String, name: String, key: String) -> Bool {
        let trimmed = key.trimmingCharacters(in: .whitespacesAndNewlines)
        let typed = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty, typed.isEmpty || !nameTaken(typed) else { return false }
        let finalName = typed.isEmpty ? defaultName(service: service) : typed
        var id = service
        while accounts.contains(where: { $0.id == id }) || id == service {
            id = "\(service)-\(String(format: "%06x", Int.random(in: 0 ..< 0xFFFFFF)))"
        }
        guard Keychain.save(id, key: trimmed) else {
            notice = "could not save the key in the Keychain"
            return false
        }
        try? engine?.setDebridAccount(id: id, service: service, key: trimmed)
        let added = DebridAccount(id: id, name: finalName, service: service)
        accounts.append(added)
        // Edit the stored list so accounts this build hides are kept.
        Prefs.saveAccounts(Self.storedAccounts().filter { $0.id != id } + [added])
        testAccount(id: id, key: trimmed)
        return true
    }

    func removeAccount(id: String) {
        guard let account = account(id) else { return }
        Keychain.delete(id)
        try? engine?.setDebridAccount(id: id, service: account.service, key: nil)
        accounts.removeAll { $0.id == id }
        Prefs.saveAccounts(Self.storedAccounts().filter { $0.id != id })
        accountNotes[id] = nil
        if debridRoute == id {
            debridRoute = nil
        }
    }

    /// Ask the provider who the key belongs to, off the main thread.
    func testAccount(id: String, key: String? = nil) {
        guard let engine, let account = account(id), let key = key ?? Keychain.read(id) else { return }
        accountNotes[id] = "checking…"
        let service = account.service
        Task.detached { [weak self] in
            let note: String
            do {
                let result = try engine.debridAccount(service: service, key: key)
                if result.premium {
                    note = "premium" + (result.expires.map { " until \($0)" } ?? "")
                } else {
                    note = "key works, but the account is not premium"
                }
            } catch {
                note = engineMessage(error)
            }
            await MainActor.run { self?.accountNotes[id] = note }
        }
    }

    func restart(job: JobView) {
        guard let engine else { return }
        do {
            _ = try engine.restart(id: job.id, seed: false, dest: Paths.publicDest().path)
            notice = nil
            refresh()
        } catch {
            notice = engineMessage(error)
        }
    }

    /// Home / app switcher. With keep alive on, the silent audio loop keeps the
    /// process running. Otherwise keep downloading for the iOS grace window, then
    /// park live jobs (stop + remember ids) before the process is suspended.
    func leaveForeground() {
        inBackground = true
        guard let engine else { return }
        engine.persist(force: true)
        // Trust only real playback: an interrupted or route-stopped loop will not keep us alive.
        if keepAliveOn && keepAlive.playing {
            return
        }
        beginGrace()
    }

    /// Back on screen: cancel any pending park and restart what was parked.
    func enterForeground() {
        inBackground = false
        graceTask?.cancel()
        graceTask = nil
        endGrace()
        resumeParked()
        syncKeepAlive()
        refresh()
    }

    private func beginGrace() {
        guard let engine, graceTask == nil, engine.anyActive() else { return }
        graceId = UIApplication.shared.beginBackgroundTask(withName: "torrenter-grace") { [weak self] in
            // Runs on main; must end the task before returning or iOS kills the app.
            MainActor.assumeIsolated {
                self?.graceTask?.cancel()
                self?.graceTask = nil
                self?.parkLiveJobs()
                self?.endGrace()
            }
        }
        let delay = BackgroundPlan.graceDelay(remaining: UIApplication.shared.backgroundTimeRemaining)
        NSLog("torrenter background: grace %.0fs", delay)
        graceTask = Task { [weak self] in
            try? await Task.sleep(nanoseconds: UInt64(delay * 1_000_000_000))
            guard !Task.isCancelled else { return }
            self?.parkLiveJobs()
            // Let engines see the stop flag and flush resume manifests.
            try? await Task.sleep(nanoseconds: 1_500_000_000)
            guard !Task.isCancelled else { return }
            self?.graceTask = nil
            self?.endGrace()
        }
    }

    private func endGrace() {
        guard graceId != .invalid else { return }
        UIApplication.shared.endBackgroundTask(graceId)
        graceId = .invalid
    }

    private func parkLiveJobs() {
        guard let engine, inBackground else { return }
        let current = engine.jobs()
        let ids = BackgroundPlan.parkable(current)
        NSLog("torrenter background: park %d job(s)", ids.count)
        Prefs.saveParked(Array(Set(Prefs.parked() + ids)))
        for job in current where job.active {
            engine.stop(id: job.id)
        }
        engine.persist(force: true)
        keepAlive.stop()
        refresh()
    }

    private func resumeParked() {
        guard let engine else { return }
        let parked = Prefs.parked()
        guard !parked.isEmpty else { return }
        let candidates = BackgroundPlan.resumable(parked: parked, jobs: engine.jobs())
        NSLog("torrenter foreground: restart %d parked job(s)", candidates.count)
        // Drop ids that are gone, done, or failed; keep the rest until each restart lands.
        var remaining = Set(candidates.map(\.id))
        Prefs.saveParked(Array(remaining))
        // Always the current Documents folder: the container path can move after a re-sideload.
        let dest = Paths.publicDest().path
        for job in candidates {
            do {
                _ = try engine.restart(id: job.id, seed: false, dest: dest)
                remaining.remove(job.id)
                Prefs.saveParked(Array(remaining))
            } catch {
                notice = engineMessage(error)
            }
        }
    }

    /// Run the silent loop only while keep alive is on and something is transferring.
    private func syncKeepAlive() {
        let want = keepAliveOn && BackgroundPlan.hasLive(engine?.jobs() ?? [])
        if want {
            guard !keepAlive.playing else { return }
            if inBackground {
                // Loop died with no notification we could act on: park the usual way.
                beginGrace()
                return
            }
            // Start while still on screen: iOS will not start audio from the background.
            guard Date() >= keepAliveRetryAt else { return }
            if keepAlive.start() {
                if keepAliveFailing && notice == Self.keepAliveNotice {
                    notice = nil
                }
                keepAliveFailing = false
            } else {
                keepAliveRetryAt = Date().addingTimeInterval(Self.keepAliveRetry)
                if !keepAliveFailing {
                    notice = Self.keepAliveNotice
                }
                keepAliveFailing = true
            }
        } else if keepAlive.wanted || keepAlive.playing {
            keepAlive.stop()
            keepAliveRetryAt = .distantPast
            // Only paused jobs left: park them the usual way instead of idling awake.
            if inBackground {
                beginGrace()
            }
        }
    }

    func shareItems(for job: JobView) -> [URL] {
        let root = URL(fileURLWithPath: job.dest)
        var isDir: ObjCBool = false
        guard FileManager.default.fileExists(atPath: root.path, isDirectory: &isDir) else {
            return []
        }
        if !isDir.boolValue {
            return [root]
        }
        let files = (try? FileManager.default.contentsOfDirectory(
            at: root,
            includingPropertiesForKeys: nil,
            options: [.skipsHiddenFiles]
        )) ?? []
        let visible = files.filter { $0.lastPathComponent != ".resume" }
        return visible.isEmpty ? [root] : visible
    }

    private func startTicker() {
        guard ticker == nil else { return }
        ticker = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: Self.tickMs * 1_000_000)
                await self?.tick()
            }
        }
    }

    private func tick() {
        guard let engine else { return }
        let notices = engine.drainNotices()
        if let last = notices.last, last != notice {
            notice = last
        }
        engine.persist(force: !engine.anyActive())
        jobList.update(engine.jobs())
        syncKeepAlive()
    }

    private func refresh() {
        jobList.update(engine?.jobs() ?? [])
    }
}

/// The live job list, observed only by the job rows.
@MainActor
final class JobList: ObservableObject {
    @Published private(set) var jobs: [JobView] = []

    /// Publish only real changes: every publish redraws its observers.
    func update(_ next: [JobView]) {
        if next != jobs {
            jobs = next
        }
    }
}

func engineMessage(_ error: Error) -> String {
    if case let .Failed(msg: msg) = error as? EngineError {
        return msg
    }
    return error.localizedDescription
}

/// A .torrent waiting for "start" (opened from another app).
struct TorrentAsk: Identifiable {
    let id = UUID()
    let data: Data
    let name: String
    let isPrivate: Bool
}
