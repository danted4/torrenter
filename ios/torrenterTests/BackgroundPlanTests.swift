import XCTest

@testable import torrenter

final class BackgroundPlanTests: XCTestCase {
    private func job(
        _ id: UInt64,
        status: String,
        active: Bool,
        paused: Bool = false,
        error: String? = nil
    ) -> JobView {
        JobView(
            id: id, kind: .http, source: "https://example.com/\(id).bin", title: "\(id).bin",
            status: status, downloaded: 0, total: 0, uploaded: 0, uploadBps: 0, peers: 0,
            speed: 0, upSpeed: 0, seed: false, dest: "/tmp", added: "2026-11-14",
            paused: paused, finished: status == "done", active: active, error: error,
            service: nil, remotePermille: 0, fileIndex: 0, fileCount: 0
        )
    }

    func testGraceDelayLeavesMarginAndCaps() {
        XCTAssertEqual(BackgroundPlan.graceDelay(remaining: 30), 20)
        XCTAssertEqual(BackgroundPlan.graceDelay(remaining: 12), 7)
        XCTAssertEqual(BackgroundPlan.graceDelay(remaining: 3), 0)
        XCTAssertEqual(BackgroundPlan.graceDelay(remaining: .greatestFiniteMagnitude), 20)
        XCTAssertEqual(BackgroundPlan.graceDelay(remaining: .infinity), 20)
    }

    func testParksRunningAndQueuedButNotPausedOrIdle() {
        let jobs = [
            job(1, status: "downloading", active: true),
            job(2, status: "queued", active: true),
            job(3, status: "paused", active: true, paused: true),
            job(4, status: "done", active: false),
            job(5, status: "stopped", active: false),
        ]
        XCTAssertEqual(BackgroundPlan.parkable(jobs), [1, 2])
    }

    func testPausedOnlyJobsDoNotCountAsLive() {
        XCTAssertFalse(BackgroundPlan.hasLive([]))
        XCTAssertFalse(BackgroundPlan.hasLive([
            job(1, status: "paused", active: true, paused: true),
            job(2, status: "done", active: false),
        ]))
        XCTAssertTrue(BackgroundPlan.hasLive([
            job(1, status: "paused", active: true, paused: true),
            job(3, status: "queued", active: true),
        ]))
    }

    func testResumesOnlyParkedJobsStillStopped() {
        let jobs = [
            job(1, status: "stopped", active: false),
            job(2, status: "done", active: false),
            job(3, status: "boom", active: false, error: "boom"),
            job(4, status: "downloading", active: true),
            job(5, status: "stopped", active: false),
        ]
        let ids = BackgroundPlan.resumable(parked: [1, 2, 3, 4, 9], jobs: jobs).map(\.id)
        XCTAssertEqual(ids, [1])
    }
}
