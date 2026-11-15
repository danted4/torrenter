package com.danted4.torrenter

import android.content.SharedPreferences
import androidx.compose.ui.graphics.Color

enum class ThemeId(val key: String) {
    Phosphor("phosphor"),
    Amber("amber"),
    Ice("ice"),
    Scarlet("scarlet"),
    Violet("violet"),
    ;

    companion object {
        fun fromName(name: String): ThemeId {
            val key = name.trim().lowercase()
            return entries.firstOrNull { it.key == key } ?: Phosphor
        }
    }
}

data class Palette(
    val bg: Color,
    val row: Color,
    val rowAlt: Color,
    val fg: Color,
    val dim: Color,
    val fail: Color,
)

fun palette(id: ThemeId): Palette =
    when (id) {
        ThemeId.Phosphor ->
            Palette(
                bg = Color(0xFF0A0A0A),
                row = Color(0xFF101010),
                rowAlt = Color(0xFF161616),
                fg = Color(0xFF6EFF6E),
                dim = Color(0xFF3E8C3E),
                fail = Color(0xFFFF5A5A),
            )
        ThemeId.Amber ->
            Palette(
                bg = Color(0xFF0C0904),
                row = Color(0xFF161006),
                rowAlt = Color(0xFF1E1608),
                fg = Color(0xFFFFB848),
                dim = Color(0xFFA87020),
                fail = Color(0xFFFF6040),
            )
        ThemeId.Ice ->
            Palette(
                bg = Color(0xFF060C12),
                row = Color(0xFF0A141E),
                rowAlt = Color(0xFF0E1C28),
                fg = Color(0xFF8CDCFF),
                dim = Color(0xFF467896),
                fail = Color(0xFFFF788C),
            )
        ThemeId.Scarlet ->
            Palette(
                bg = Color(0xFF0A0608),
                row = Color(0xFF140A0E),
                rowAlt = Color(0xFF1C0E12),
                fg = Color(0xFFFF5866),
                dim = Color(0xFFB03440),
                fail = Color(0xFFFFA848),
            )
        ThemeId.Violet ->
            Palette(
                bg = Color(0xFF0A080E),
                row = Color(0xFF120E1A),
                rowAlt = Color(0xFF1A1424),
                fg = Color(0xFFD2BAFF),
                dim = Color(0xFF947CC4),
                fail = Color(0xFFFF8C60),
            )
    }

object Prefs {
    const val NAME = "torrenter"
    const val THEME = "theme"
    const val SEED = "seed"

    fun theme(prefs: SharedPreferences): ThemeId =
        ThemeId.fromName(prefs.getString(THEME, ThemeId.Phosphor.key) ?: ThemeId.Phosphor.key)

    fun saveTheme(
        prefs: SharedPreferences,
        id: ThemeId,
    ) {
        prefs.edit().putString(THEME, id.key).apply()
    }

    fun seed(prefs: SharedPreferences): Boolean = prefs.getBoolean(SEED, true)

    const val DEBRID_ROUTE = "debridRoute"
    const val CONNECTIONS = "connections"
    const val REMOVE_AFTER = "debridRemoveAfter"

    /** Remembered debrid service for magnet jobs; null is off. */
    fun debridRoute(prefs: SharedPreferences): String? = prefs.getString(DEBRID_ROUTE, null)

    fun saveDebridRoute(
        prefs: SharedPreferences,
        id: String?,
    ) {
        prefs.edit().apply { if (id == null) remove(DEBRID_ROUTE) else putString(DEBRID_ROUTE, id) }.apply()
    }

    /** Connections per download; 0 is Auto (4). */
    fun connections(prefs: SharedPreferences): Int = prefs.getInt(CONNECTIONS, 0)

    fun saveConnections(
        prefs: SharedPreferences,
        parts: Int,
    ) {
        prefs.edit().putInt(CONNECTIONS, parts).apply()
    }

    fun removeAfter(prefs: SharedPreferences): Boolean = prefs.getBoolean(REMOVE_AFTER, false)

    fun saveRemoveAfter(
        prefs: SharedPreferences,
        on: Boolean,
    ) {
        prefs.edit().putBoolean(REMOVE_AFTER, on).apply()
    }

    fun connectionsArg(parts: Int): UByte? = if (parts > 0) parts.coerceAtMost(16).toUByte() else null

    fun saveSeed(
        prefs: SharedPreferences,
        seed: Boolean,
    ) {
        prefs.edit().putBoolean(SEED, seed).apply()
    }
}
