package com.danted4.torrenter

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Test

class ThemeTest {
    @Test
    fun unknownNameFallsBackToPhosphor() {
        assertEquals(ThemeId.Phosphor, ThemeId.fromName("blood"))
        assertEquals(ThemeId.Phosphor, ThemeId.fromName(""))
        assertEquals(ThemeId.Amber, ThemeId.fromName("  AMBER "))
        assertEquals(ThemeId.Violet, ThemeId.fromName("violet"))
    }

    @Test
    fun palettesAreDistinct() {
        assertNotEquals(palette(ThemeId.Phosphor).fg, palette(ThemeId.Amber).fg)
        assertNotEquals(palette(ThemeId.Ice).fg, palette(ThemeId.Scarlet).fg)
        assertNotEquals(palette(ThemeId.Violet).fg, palette(ThemeId.Phosphor).fg)
    }

    @Test
    fun downloadsAtOnceStayInRange() {
        assertEquals(1, Prefs.maxDownloadsArg(0).toInt())
        assertEquals(4, Prefs.maxDownloadsArg(4).toInt())
        assertEquals(8, Prefs.maxDownloadsArg(99).toInt())
    }
}
