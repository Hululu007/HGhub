package com.hghub.player

import android.app.Activity
import android.content.pm.ActivityInfo
import androidx.annotation.Keep
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.Plugin

@InvokeArg
@Keep
class OrientationArgs {
  var landscape: Boolean = false
}

@TauriPlugin
@Keep
class OrientationPlugin(private val activity: Activity) : Plugin(activity) {
  // Edge-to-edge：让 WebView 铺满整个屏幕到底部导航栏下方，而不是被系统
  // 留出一截白色窗口底。导航栏/状态栏改为透明，真正的留白只由前端
  // env(safe-area-inset-bottom)（--sab，已上限 48px）承担，不再双重留白。
  init {
    activity.runOnUiThread {
      WindowCompat.setDecorFitsSystemWindows(activity.window, false)
      activity.window.statusBarColor = android.graphics.Color.TRANSPARENT
      activity.window.navigationBarColor = android.graphics.Color.TRANSPARENT
      if (android.os.Build.VERSION.SDK_INT >= android.os.Build.VERSION_CODES.Q) {
        activity.window.isNavigationBarContrastEnforced = false
      }
    }
  }

  private var previousOrientation: Int? = null

  @Command
  fun setOrientation(invoke: Invoke) {
    val args = try {
      invoke.parseArgs(OrientationArgs::class.java)
    } catch (error: Exception) {
      invoke.reject(error.message)
      return
    }
    activity.runOnUiThread {
      try {
        val insets = WindowCompat.getInsetsController(activity.window, activity.window.decorView)
        if (args.landscape) {
          if (previousOrientation == null) previousOrientation = activity.requestedOrientation
          activity.requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_SENSOR_LANDSCAPE
          insets.systemBarsBehavior = WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
          insets.hide(WindowInsetsCompat.Type.systemBars())
        } else if (previousOrientation != null) {
          activity.requestedOrientation = previousOrientation!!
          previousOrientation = null
          insets.show(WindowInsetsCompat.Type.systemBars())
        }
        invoke.resolve()
      } catch (error: Exception) {
        invoke.reject(error.message)
      }
    }
  }
}
