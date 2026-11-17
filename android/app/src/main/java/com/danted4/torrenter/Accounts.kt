package com.danted4.torrenter

import android.content.Context
import org.json.JSONArray
import org.json.JSONObject
import uniffi.torrenter.debridServices

/** A named key for one debrid provider; a provider can have several. */
data class DebridAccount(
    /** Stable id the engine, jobs and the remembered route use. Keys saved
     *  before accounts existed use the provider id. */
    val id: String,
    val name: String,
    val service: String,
)

/**
 * Debrid accounts: names and providers in the app's prefs, each key
 * encrypted by [SecureKeys] under the account id.
 */
object Accounts {
    private const val KEY = "debridAccounts"

    /**
     * Accounts this build can use. A key saved per provider before accounts
     * existed becomes an account with that id. An entry is dropped from the
     * stored list only when its key is provably gone ([SecureKeys.has]); a
     * transient Keystore failure or an unknown provider never removes it, and
     * the offered-provider filter applies in memory only.
     */
    fun load(context: Context): List<DebridAccount> {
        val stored = stored(context)
        val services = runCatching { debridServices() }.getOrNull()
        val list = stored.toMutableList()
        for (service in services.orEmpty()) {
            if (list.none { it.id == service.id } && SecureKeys.has(context, service.id)) {
                list += DebridAccount(service.id, service.label, service.id)
            }
        }
        val kept = list.filter { SecureKeys.has(context, it.id) }
        if (kept != stored) save(context, kept)
        val offered = services?.map { it.id }?.toSet() ?: return kept
        return kept.filter { it.service in offered }
    }

    /** Add one account to the stored list (others, even hidden ones, kept). */
    fun add(
        context: Context,
        account: DebridAccount,
    ) = save(context, stored(context).filterNot { it.id == account.id } + account)

    fun remove(
        context: Context,
        id: String,
    ) = save(context, stored(context).filterNot { it.id == id })

    private fun stored(context: Context): List<DebridAccount> =
        parse(
            context.getSharedPreferences(Prefs.NAME, Context.MODE_PRIVATE).getString(KEY, null),
        ).distinctBy { it.id }

    fun save(
        context: Context,
        accounts: List<DebridAccount>,
    ) {
        val array = JSONArray()
        for (account in accounts) {
            array.put(
                JSONObject()
                    .put("id", account.id)
                    .put("name", account.name)
                    .put("service", account.service),
            )
        }
        context
            .getSharedPreferences(Prefs.NAME, Context.MODE_PRIVATE)
            .edit()
            .putString(KEY, array.toString())
            .apply()
    }

    private fun parse(text: String?): List<DebridAccount> {
        if (text.isNullOrEmpty()) return emptyList()
        return runCatching {
            val array = JSONArray(text)
            (0 until array.length()).map { index ->
                val item = array.getJSONObject(index)
                DebridAccount(item.getString("id"), item.getString("name"), item.getString("service"))
            }
        }.getOrDefault(emptyList())
    }

    /** Names are unique across providers, ignoring case. */
    fun nameTaken(
        name: String,
        accounts: List<DebridAccount>,
    ): Boolean {
        val wanted = name.trim().lowercase()
        return accounts.any { it.name.lowercase() == wanted }
    }

    /** The name a new account gets when none is typed: the provider's label,
     *  then "TorBox 2", … */
    fun defaultName(
        label: String,
        accounts: List<DebridAccount>,
    ): String {
        if (!nameTaken(label, accounts)) return label
        var n = 2
        while (nameTaken("$label $n", accounts)) n += 1
        return "$label $n"
    }

    /** A fresh id like `torbox-1a2b3c`, never the bare provider id. */
    fun newId(
        service: String,
        accounts: List<DebridAccount>,
    ): String {
        while (true) {
            val id = "$service-" + "%06x".format(kotlin.random.Random.nextInt(0, 0xFFFFFF))
            if (accounts.none { it.id == id }) return id
        }
    }
}
