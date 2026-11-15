package com.danted4.torrenter

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyPermanentlyInvalidatedException
import android.security.keystore.KeyProperties
import android.util.Base64
import java.security.KeyStore
import javax.crypto.AEADBadTagException
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * Debrid API keys, encrypted with an AES-GCM key that lives in the Android
 * Keystore (it never leaves secure hardware). The ciphertext sits in its own
 * private prefs file, excluded from backups (see res/xml backup rules). The
 * engine only ever gets a key in memory at runtime.
 */
object SecureKeys {
    private const val ALIAS = "torrenter.debrid"
    const val FILE = "torrenter_keys"
    private const val TRANSFORM = "AES/GCM/NoPadding"
    private const val TAG_BITS = 128

    fun read(
        context: Context,
        service: String,
    ): String? {
        val stored = prefs(context).getString(service, null) ?: return null
        val parts = stored.split(":", limit = 2)
        val decoded =
            try {
                parts.map { Base64.decode(it, Base64.NO_WRAP) }
            } catch (_: IllegalArgumentException) {
                null
            }
        if (decoded == null || decoded.size != 2) {
            // Malformed value: it can never decrypt.
            delete(context, service)
            return null
        }
        val (iv, body) = decoded
        return try {
            val cipher = Cipher.getInstance(TRANSFORM)
            cipher.init(Cipher.DECRYPT_MODE, secretKey(), GCMParameterSpec(TAG_BITS, iv))
            String(cipher.doFinal(body), Charsets.UTF_8).ifEmpty { null }
        } catch (error: Exception) {
            // Only errors that prove the key is unusable (restored from another
            // device, Keystore key reset or invalidated) drop it. A transient
            // Keystore failure (daemon busy, right after boot) keeps it for
            // the next read.
            if (error is AEADBadTagException || error is KeyPermanentlyInvalidatedException) {
                delete(context, service)
            }
            null
        }
    }

    fun save(
        context: Context,
        service: String,
        key: String,
    ): Boolean =
        try {
            val cipher = Cipher.getInstance(TRANSFORM)
            cipher.init(Cipher.ENCRYPT_MODE, secretKey())
            val body = cipher.doFinal(key.toByteArray(Charsets.UTF_8))
            val encoded =
                Base64.encodeToString(cipher.iv, Base64.NO_WRAP) + ":" +
                    Base64.encodeToString(body, Base64.NO_WRAP)
            prefs(context).edit().putString(service, encoded).commit()
        } catch (_: Exception) {
            false
        }

    /** A stored (encrypted) key exists for this id, without decrypting it.
     *  False only when it is gone (never saved, removed, or dropped by [read]
     *  as unusable). */
    fun has(
        context: Context,
        id: String,
    ): Boolean = prefs(context).contains(id)

    fun delete(
        context: Context,
        service: String,
    ) {
        prefs(context).edit().remove(service).apply()
    }

    private fun prefs(context: Context) = context.getSharedPreferences(FILE, Context.MODE_PRIVATE)

    private fun secretKey(): SecretKey {
        val store = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        (store.getEntry(ALIAS, null) as? KeyStore.SecretKeyEntry)?.let { return it.secretKey }
        val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        generator.init(
            KeyGenParameterSpec
                .Builder(ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                .build(),
        )
        return generator.generateKey()
    }
}
