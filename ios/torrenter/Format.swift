import Foundation

func formatSize(_ size: UInt64) -> String {
    if size == 0 {
        return "0.00 B"
    }
    return formatMagnitude(Double(size), bytes: true)
}

func formatSpeed(_ speed: Double) -> String {
    if !speed.isFinite || speed <= 0 {
        return "0.00 B/s"
    }
    return formatMagnitude(speed, bytes: false)
}

func formatEta(_ seconds: Double?) -> String {
    guard let seconds, seconds.isFinite, seconds >= 0 else {
        return "N/A"
    }
    let whole = Int64(seconds)
    if whole == Int64.max {
        return "N/A"
    }
    let minutes = whole / 60
    let hours = minutes / 60
    let days = hours / 24
    let weeks = days / 7
    let months = Int64(Double(weeks) / 4.345)
    let years = months / 12
    if years > 0 { return plural(years, "year") }
    if months > 0 { return plural(months, "month") }
    if weeks > 0 { return plural(weeks, "week") }
    if days > 0 { return plural(days, "day") }
    if hours > 0 { return plural(hours, "hour") }
    if minutes > 0 { return plural(minutes, "minute") }
    return plural(whole, "second")
}

/// The debrid service is still fetching (before files download).
func jobIsFetching(_ job: JobView) -> Bool {
    job.service != nil && ["fetching", "queued", "starting"].contains(job.status)
}

func serviceLabel(_ id: String?) -> String {
    guard let id else { return "" }
    return debridServices().first { $0.id == id }?.label ?? id
}

/// Row badge: MAGNET, HTTP, or the debrid phase.
func jobBadge(_ job: JobView) -> String {
    if let service = job.service {
        return jobIsFetching(job) ? "MAGNET → \(serviceLabel(service))" : "HTTP · via \(serviceLabel(service))"
    }
    return job.kind == .http ? "HTTP" : "MAGNET"
}

func jobStatsLine(_ job: JobView) -> String {
    let downloaded = job.downloaded
    let total = job.total
    let eta: String
    if !job.paused, job.speed > 1.0, total > downloaded {
        eta = formatEta(Double(total - downloaded) / job.speed)
    } else {
        eta = "N/A"
    }
    if let service = job.service {
        if jobIsFetching(job) {
            let pct = String(format: "%.1f", locale: Locale(identifier: "en_US_POSIX"), Double(job.remotePermille) / 10.0)
            return "\(job.status)  \(serviceLabel(service)) fetching \(pct)%"
        }
        let files = job.fileCount > 0 ? "  file \(max(job.fileIndex, 1))/\(job.fileCount)" : ""
        return "\(job.status)\(files)  D \(formatSize(downloaded)) @ \(formatSpeed(job.speed))  \(formatSize(total))  eta \(eta)"
    }
    if job.kind == .http {
        return "\(job.status)  D \(formatSize(downloaded)) @ \(formatSpeed(job.speed))  \(formatSize(total))  eta \(eta)"
    }
    let sharing = job.kind == .magnet && job.seed
    let seeding = sharing && job.active && total > 0 && downloaded >= total
    let upSpeed = job.upSpeed > 0.0 ? job.upSpeed : Double(job.uploadBps)
    if sharing {
        if seeding {
            return "\(job.status)  peers \(job.peers)  D \(formatSize(downloaded))  U \(formatSize(job.uploaded)) @ \(formatSpeed(upSpeed))  \(formatSize(total))"
        }
        return "\(job.status)  peers \(job.peers)  D \(formatSize(downloaded)) @ \(formatSpeed(job.speed))  U \(formatSize(job.uploaded)) @ \(formatSpeed(upSpeed))  \(formatSize(total))  eta \(eta)"
    }
    return "\(job.status)  peers \(job.peers)  D \(formatSize(downloaded)) @ \(formatSpeed(job.speed))  \(formatSize(total))  eta \(eta)"
}

func jobBarFraction(_ job: JobView) -> Float? {
    let status = job.status
    if job.service != nil, status == "fetching" {
        return Float(job.remotePermille) / 1000
    }
    if status == "queued" || status == "starting" || status.contains("Initializing") {
        return nil
    }
    if job.total <= 1, job.downloaded > job.total {
        return nil
    }
    if job.total == 0 {
        return 0
    }
    return Float((Double(job.downloaded) / Double(job.total)).clamped(to: 0 ... 1))
}

private func plural(_ n: Int64, _ unit: String) -> String {
    n == 1 ? "1 \(unit)" : "\(n) \(unit)s"
}

private func formatMagnitude(_ value: Double, bytes: Bool) -> String {
    let level = min(max(Int(floor(log(value) / log(1024.0))), 0), 3)
    let scaled = value / pow(1024.0, Double(level))
    let pattern: String
    switch (level, bytes) {
    case (0, _): pattern = "%.2f B"
    case (1, _): pattern = "%.2f KB"
    case (2, true): pattern = "%.3f MB"
    case (_, true): pattern = "%.4f GB"
    case (2, false): pattern = "%.2f MB"
    default: pattern = "%.2f GB"
    }
    let body = String(format: pattern, locale: Locale(identifier: "en_US_POSIX"), scaled)
    return bytes ? body : "\(body)/s"
}

private extension Comparable {
    func clamped(to range: ClosedRange<Self>) -> Self {
        min(max(self, range.lowerBound), range.upperBound)
    }
}
