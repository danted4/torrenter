import XCTest

@testable import torrenter

final class FormatTests: XCTestCase {
    func testFormatsZeroAndKnownSizes() {
        XCTAssertEqual(formatSize(0), "0.00 B")
        XCTAssertEqual(formatSize(512), "512.00 B")
        XCTAssertEqual(formatSize(1024), "1.00 KB")
        XCTAssertEqual(formatSize(1024 * 1024), "1.000 MB")
        XCTAssertEqual(formatSize(1024 * 1024 * 1024), "1.0000 GB")
    }

    func testFormatsSpeedAndEtaUnits() {
        XCTAssertEqual(formatSpeed(0), "0.00 B/s")
        XCTAssertEqual(formatSpeed(.nan), "0.00 B/s")
        XCTAssertEqual(formatSpeed(1024), "1.00 KB/s")
        XCTAssertEqual(formatEta(nil), "N/A")
        XCTAssertEqual(formatEta(1), "1 second")
        XCTAssertEqual(formatEta(2), "2 seconds")
        XCTAssertEqual(formatEta(60), "1 minute")
        XCTAssertEqual(formatEta(3600), "1 hour")
        XCTAssertEqual(formatEta(86_400), "1 day")
    }

    private func job(
        kind: Kind,
        status: String,
        service: String? = nil,
        permille: UInt16 = 0,
        fileIndex: UInt32 = 0,
        fileCount: UInt32 = 0
    ) -> JobView {
        JobView(
            id: 1, kind: kind, source: "magnet:?xt=urn:btih:x", title: "x", status: status,
            downloaded: 0, total: 0, uploaded: 0, uploadBps: 0, peers: 3, speed: 0, upSpeed: 0,
            seed: false, dest: "/tmp", added: "2026-11-14", paused: false, finished: false,
            active: true, error: nil, service: service, remotePermille: permille,
            fileIndex: fileIndex, fileCount: fileCount
        )
    }

    func testDebridRowsShowPhasesWithoutPeers() {
        let fetching = job(kind: .debrid, status: "fetching", service: "torbox", permille: 452)
        XCTAssertTrue(jobIsFetching(fetching))
        XCTAssertEqual(jobBadge(fetching), "MAGNET → TorBox")
        XCTAssertEqual(jobStatsLine(fetching), "fetching  TorBox fetching 45.2%")
        XCTAssertEqual(jobBarFraction(fetching), 0.452)

        let downloading = job(kind: .debrid, status: "downloading", service: "torbox", fileIndex: 2, fileCount: 7)
        XCTAssertFalse(jobIsFetching(downloading))
        XCTAssertEqual(jobBadge(downloading), "HTTP · via TorBox")
        XCTAssertTrue(jobStatsLine(downloading).hasPrefix("downloading  file 2/7  D "))
        XCTAssertFalse(jobStatsLine(downloading).contains("peers"))

        let http = job(kind: .http, status: "downloading")
        XCTAssertFalse(jobStatsLine(http).contains("peers"))
        XCTAssertEqual(jobBadge(http), "HTTP")
        XCTAssertTrue(jobStatsLine(job(kind: .magnet, status: "downloading")).contains("peers 3"))
    }
}
