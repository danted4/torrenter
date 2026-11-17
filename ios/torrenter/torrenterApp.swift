import SwiftUI

@main
struct TorrenterApp: App {
    @StateObject private var owner = EngineOwner()

    var body: some Scene {
        WindowGroup {
            ContentView()
                .environmentObject(owner)
                // "Open with torrenter" on a .torrent (Files, Safari).
                .onOpenURL { url in
                    if url.isFileURL {
                        owner.queueTorrentFile(at: url)
                    }
                }
        }
    }
}
