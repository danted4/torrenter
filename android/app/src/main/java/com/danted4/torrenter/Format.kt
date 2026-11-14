package com.danted4.torrenter

import java.util.Locale
import kotlin.math.floor
import kotlin.math.ln
import uniffi.torrenter.JobView
import uniffi.torrenter.Kind
import uniffi.torrenter.debridServices

fun formatSize(size: ULong): String {
    if (size == 0uL) {
        return "0.00 B"
    }
    return formatMagnitude(size.toDouble(), bytes = true)
}

fun formatSpeed(speed: Double): String {
    if (!speed.isFinite() || speed <= 0.0) {
        return "0.00 B/s"
    }
    return formatMagnitude(speed, bytes = false)
}

fun formatEta(seconds: Double?): String {
    if (seconds == null || !seconds.isFinite() || seconds < 0.0) {
        return "N/A"
    }
    val whole = seconds.toLong()
    if (whole == Long.MAX_VALUE) {
        return "N/A"
    }
    val minutes = whole / 60
    val hours = minutes / 60
    val days = hours / 24
    val weeks = days / 7
    val months = (weeks.toDouble() / 4.345).toLong()
    val years = months / 12
    return when {
        years > 0 -> plural(years, "year")
        months > 0 -> plural(months, "month")
        weeks > 0 -> plural(weeks, "week")
        days > 0 -> plural(days, "day")
        hours > 0 -> plural(hours, "hour")
        minutes > 0 -> plural(minutes, "minute")
        else -> plural(whole, "second")
    }
}

/** The debrid service is still fetching (before files download). */
fun jobIsFetching(job: JobView): Boolean = job.service != null && job.status in setOf("fetching", "queued", "starting")

fun serviceLabel(id: String?): String =
    id?.let { wanted -> runCatching { debridServices().firstOrNull { it.id == wanted }?.label }.getOrNull() ?: wanted } ?: ""

/** Row badge: MAGNET, HTTP, or the debrid phase. */
fun jobBadge(job: JobView): String {
    val service = job.service ?: return if (job.kind == Kind.HTTP) "HTTP" else "MAGNET"
    return if (jobIsFetching(job)) "MAGNET → ${serviceLabel(service)}" else "HTTP · via ${serviceLabel(service)}"
}

fun jobStatsLine(job: JobView): String {
    val downloaded = job.downloaded
    val total = job.total
    val eta =
        if (!job.paused && job.speed > 1.0 && total > downloaded) {
            formatEta((total - downloaded).toDouble() / job.speed)
        } else {
            "N/A"
        }
    job.service?.let { service ->
        if (jobIsFetching(job)) {
            val pct = String.format(java.util.Locale.US, "%.1f", job.remotePermille.toDouble() / 10.0)
            return "${job.status}  ${serviceLabel(service)} fetching $pct%"
        }
        val files = if (job.fileCount > 0u) "  file ${maxOf(job.fileIndex, 1u)}/${job.fileCount}" else ""
        return "${job.status}$files  D ${formatSize(downloaded)} @ ${formatSpeed(job.speed)}  ${formatSize(total)}  eta $eta"
    }
    if (job.kind == Kind.HTTP) {
        return "${job.status}  D ${formatSize(downloaded)} @ ${formatSpeed(job.speed)}  ${formatSize(total)}  eta $eta"
    }
    val sharing = job.kind == Kind.MAGNET && job.seed
    val seeding = sharing && job.active && total > 0uL && downloaded >= total
    val upSpeed = if (job.upSpeed > 0.0) job.upSpeed else job.uploadBps.toDouble()
    return if (sharing) {
        if (seeding) {
            "${job.status}  peers ${job.peers}  D ${formatSize(downloaded)}  U ${formatSize(job.uploaded)} @ ${formatSpeed(upSpeed)}  ${formatSize(total)}"
        } else {
            "${job.status}  peers ${job.peers}  D ${formatSize(downloaded)} @ ${formatSpeed(job.speed)}  U ${formatSize(job.uploaded)} @ ${formatSpeed(upSpeed)}  ${formatSize(total)}  eta $eta"
        }
    } else {
        "${job.status}  peers ${job.peers}  D ${formatSize(downloaded)} @ ${formatSpeed(job.speed)}  ${formatSize(total)}  eta $eta"
    }
}

private fun plural(
    n: Long,
    unit: String,
): String = if (n == 1L) "1 $unit" else "$n ${unit}s"

private fun formatMagnitude(
    value: Double,
    bytes: Boolean,
): String {
    val level = floor(ln(value) / ln(1024.0)).toInt().coerceIn(0, 3)
    val scaled = value / Math.pow(1024.0, level.toDouble())
    val pattern =
        when {
            level == 0 -> "%.2f B"
            level == 1 -> "%.2f KB"
            bytes && level == 2 -> "%.3f MB"
            bytes -> "%.4f GB"
            level == 2 -> "%.2f MB"
            else -> "%.2f GB"
        }
    val body = String.format(Locale.US, pattern, scaled)
    return if (bytes) body else "$body/s"
}
