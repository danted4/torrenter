plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "com.danted4.torrenter"
    compileSdk = 35

    defaultConfig {
        applicationId = "com.danted4.torrenter"
        minSdk = 30
        targetSdk = 35
        versionCode = 17
        versionName = "3.10.1"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        ndk {
            abiFilters += listOf("arm64-v8a", "x86_64")
        }
    }

    // Release APKs are signed with the project's own key, read from the
    // environment (release.yml decodes it from GitHub secrets). The key never
    // lives in the repo; without it a release build is left unsigned and the
    // release workflow refuses to ship it.
    val keystore = System.getenv("TORRENTER_KEYSTORE")?.takeIf { it.isNotBlank() }
    signingConfigs {
        if (keystore != null) {
            create("release") {
                storeFile = file(keystore)
                storePassword = System.getenv("TORRENTER_KEYSTORE_PASSWORD")
                keyAlias = System.getenv("TORRENTER_KEY_ALIAS")
                // PKCS12 keystores use one password for the store and the key.
                keyPassword =
                    System.getenv("TORRENTER_KEY_PASSWORD")?.takeIf { it.isNotBlank() }
                        ?: System.getenv("TORRENTER_KEYSTORE_PASSWORD")
            }
        }
    }

    buildTypes {
        release {
            if (keystore != null) {
                signingConfig = signingConfigs.getByName("release")
            }
            isMinifyEnabled = false
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro",
            )
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions {
        jvmTarget = "17"
    }
    buildFeatures {
        compose = true
    }
    packaging {
        jniLibs {
            useLegacyPackaging = false
        }
    }
}

dependencies {
    implementation(platform("androidx.compose:compose-bom:2024.10.01"))
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-tooling-preview")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.core:core-ktx:1.15.0")
    implementation("androidx.activity:activity-compose:1.9.3")
    implementation("androidx.lifecycle:lifecycle-runtime-ktx:2.8.7")
    implementation("net.java.dev.jna:jna:5.15.0@aar")
    testImplementation("junit:junit:4.13.2")
    debugImplementation("androidx.compose.ui:ui-tooling")
}
