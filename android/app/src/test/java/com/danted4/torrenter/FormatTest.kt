package com.danted4.torrenter

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import uniffi.torrenter.JobView
import uniffi.torrenter.Kind

class FormatTest {
    @Test
    fun formatsZeroAndKnownSizes() {
        assertEquals("0.00 B", formatSize(0uL))
        assertEquals("512.00 B", formatSize(512uL))
        assertEquals("1.00 KB", formatSize(1024uL))
        assertEquals("1.000 MB", formatSize(1024uL * 1024uL))
        assertEquals("1.0000 GB", formatSize(1024uL * 1024uL * 1024uL))
    }

    @Test
    fun formatsSpeedAndEtaUnits() {
        assertEquals("0.00 B/s", formatSpeed(0.0))
        assertEquals("0.00 B/s", formatSpeed(Double.NaN))
        assertEquals("1.00 KB/s", formatSpeed(1024.0))
        assertEquals("N/A", formatEta(null))
        assertEquals("1 second", formatEta(1.0))
        assertEquals("2 seconds", formatEta(2.0))
        assertEquals("1 minute", formatEta(60.0))
        assertEquals("1 hour", formatEta(3600.0))
        assertEquals("1 day", formatEta(86400.0))
    }

    @Test
    fun httpJobLineMatchesDesktop() {
        val line =
            jobStatsLine(
                sample(
                    kind = Kind.HTTP,
                    seed = false,
                    downloaded = 1024uL,
                    total = 2048uL,
                    speed = 1024.0,
                ),
            )
        assertEquals("downloading  D 1.00 KB @ 1.00 KB/s  2.00 KB  eta 1 second", line)
    }

    @Test
    fun debridLinesShowPhasesWithoutPeers() {
        val base = sample(kind = Kind.DEBRID, seed = false, downloaded = 1024uL, total = 2048uL, speed = 1024.0)
        // JVM tests cannot load the native library, so the label falls back to the id.
        val fetching = base.copy(status = "fetching", service = "torbox", remotePermille = 452u)
        assertTrue(jobIsFetching(fetching))
        assertEquals("fetching  torbox fetching 45.2%", jobStatsLine(fetching))
        assertEquals("MAGNET → torbox", jobBadge(fetching))
        val downloading = base.copy(service = "torbox", fileIndex = 2u, fileCount = 7u)
        assertEquals("downloading  file 2/7  D 1.00 KB @ 1.00 KB/s  2.00 KB  eta 1 second", jobStatsLine(downloading))
        assertEquals("HTTP · via torbox", jobBadge(downloading))
    }

    @Test
    fun debridRowsUseTheAccountName() {
        val names = mapOf("torbox-1a2b3c" to "work")
        val base = sample(kind = Kind.DEBRID, seed = false, downloaded = 0uL, total = 0uL, speed = 0.0)
        val fetching = base.copy(status = "fetching", service = "torbox", remotePermille = 100u, account = "torbox-1a2b3c")
        assertEquals("MAGNET → work", jobBadge(fetching, names))
        assertEquals("fetching  work fetching 10.0%", jobStatsLine(fetching, names))
        // An unknown account (removed since) falls back to the provider id.
        val orphan = base.copy(service = "torbox", account = "gone")
        assertEquals("HTTP · via torbox", jobBadge(orphan, names))
    }

    @Test
    fun accountNamesStayUniqueAndDefaultToTheProvider() {
        val taken = listOf(DebridAccount("torbox", "TorBox", "torbox"), DebridAccount("torbox-1", "work", "torbox"))
        assertTrue(Accounts.nameTaken("WORK", taken))
        assertEquals("TorBox 2", Accounts.defaultName("TorBox", taken))
        assertEquals("Real-Debrid", Accounts.defaultName("Real-Debrid", taken))
        assertTrue(Accounts.newId("torbox", taken).startsWith("torbox-"))
    }

    @Test
    fun magnetShareLineIncludesUpload() {
        val line =
            jobStatsLine(
                sample(
                    kind = Kind.MAGNET,
                    seed = true,
                    downloaded = 512uL,
                    total = 1024uL,
                    uploaded = 256uL,
                    speed = 512.0,
                    upSpeed = 256.0,
                    peers = 3uL,
                ),
            )
        assertEquals(
            "downloading  peers 3  D 512.00 B @ 512.00 B/s  U 256.00 B @ 256.00 B/s  1.00 KB  eta 1 second",
            line,
        )
    }

    @Test
    fun aPartialSelectionIsCountedOnTheRow() {
        assertEquals("", filesHint(0u, 0u))
        assertEquals("", filesHint(1u, 1u))
        assertEquals("", filesHint(20u, 20u))
        assertEquals("  4 of 20 files", filesHint(20u, 4u))
        assertEquals("  no files selected", filesHint(20u, 0u))
        val job = sample(Kind.MAGNET, seed = false, downloaded = 0uL, total = 0uL).copy(filesTotal = 20u, filesSelected = 4u)
        assertTrue(jobStatsLine(job).endsWith("  4 of 20 files"))
        // HTTP jobs never count files.
        assertTrue(!jobStatsLine(sample(Kind.HTTP, seed = false, downloaded = 0uL, total = 0uL)).contains("files"))
    }

    @Test
    fun fileSearchChecksAndToggles() {
        assertTrue(fileMatches("Season 1", "E05.mkv", "e05"))
        assertTrue(fileMatches("Season 1", "E05.mkv", "season"))
        assertTrue(fileMatches("", "readme.nfo", "  "))
        assertTrue(!fileMatches("Season 1", "E05.mkv", "E06"))
        assertEquals(Check.OFF, checkOf(0, 3))
        assertEquals(Check.SOME, checkOf(2, 3))
        assertEquals(Check.ON, checkOf(3, 3))
        assertEquals(Check.OFF, checkOf(0, 0))
        assertEquals(listOf(0u, 1u, 3u), toggledFiles(setOf(3u, 0u), listOf(1u), true))
        assertEquals(listOf(3u), toggledFiles(setOf(3u, 0u, 1u), listOf(0u, 1u), false))
        assertEquals(1f, fileFraction(0uL, 0uL))
        assertEquals(0.5f, fileFraction(5uL, 10uL))
        assertEquals(1f, fileFraction(20uL, 10uL))
    }

    private fun sample(
        kind: Kind,
        seed: Boolean,
        downloaded: ULong,
        total: ULong,
        uploaded: ULong = 0uL,
        speed: Double = 0.0,
        upSpeed: Double = 0.0,
        peers: ULong = 0uL,
    ): JobView =
        JobView(
            id = 1uL,
            kind = kind,
            source = "https://example.com/file",
            title = "file",
            status = "downloading",
            downloaded = downloaded,
            total = total,
            uploaded = uploaded,
            uploadBps = 0uL,
            peers = peers,
            speed = speed,
            upSpeed = upSpeed,
            seed = seed,
            dest = "/tmp",
            added = "2026-11-07",
            paused = false,
            finished = false,
            active = true,
            error = null,
            service = null,
            remotePermille = 0u,
            fileIndex = 0u,
            fileCount = 0u,
        )
}
