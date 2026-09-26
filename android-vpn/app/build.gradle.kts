plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.android)
}

android {
    // Имя пакета входит в имена JNI-функций libhomeproxy.so (Java_ru_homeproxy_...):
    // менять его можно только вместе с hp-backend/android-lib.
    namespace = "ru.homeproxy"
    compileSdk = 36

    defaultConfig {
        applicationId = "ru.homeproxy"
        minSdk = 24
        targetSdk = 35
        versionCode = 1
        versionName = "0.1"

        // Нативные библиотеки собирает build-native.sh в jniLibs/<abi>.
        ndk {
            abiFilters += listOf("armeabi-v7a", "x86_64")
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_11
        targetCompatibility = JavaVersion.VERSION_11
    }

    kotlinOptions {
        jvmTarget = "11"
    }
}

dependencies {
    testImplementation(libs.junit)
}
