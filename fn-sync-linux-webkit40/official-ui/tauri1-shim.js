// Tauri1 桥接层(非业务逻辑):
// 官方前端 bundle 按 Tauri2 运行时打包(依赖 window.__TAURI_INTERNALS__ / plugin:window|* 等
// v2 专有接口)。Tauri1(withGlobalTauri)注入的是 window.__TAURI__,本文件在页面脚本之前
// 同步执行,把 v2 的调用面完整翻译到 v1;后端命令与业务语义不变。
// 由 index.html <head> 内 <script src="tauri1-shim.js"> 加载,勿异步、勿 defer。
(function () {
  if (window.__TAURI_INTERNALS__) return;
  var T = window.__TAURI__;
  if (!T) return;

  var TAURI = T.tauri || T;
  var WIN = T.window;
  var EV = T.event;
  var DLG = T.dialog || {};
  var SH = T.shell || {};

  var cbId = 0;
  var cbMap = {};        // handler id -> callback(transformCallback 注册)
  var unlistenMap = {};  // event listener id -> v1 unlisten 函数
  var seq = 0;

  var cur = function () { return WIN.getCurrent(); };

  // ---- size/position 归一化:v2 {value:{Logical|Physical:{...}}} -> v1 构造器 ----
  // v2 的 invoke 参数是 {label, value}；兼容早期误用的 {data} 与扁平写法。
  function v1Size(a, ctorL, ctorP) {
    var d = a && (a.value !== undefined ? a.value : a.data);
    if (!d) return null;
    if (d.Logical) return new ctorL(d.Logical.width, d.Logical.height);
    if (d.Physical) return new ctorP(d.Physical.width, d.Physical.height);
    if (d.width != null || d.x != null) return new ctorL(d.width != null ? d.width : d.x, d.height != null ? d.height : d.y);
    return null;
  }

  // ---- plugin:window|* 命令映射(v1 Window API) ----
  // 官方 bundle 引用的全部命令;v1 不支持的桌面能力(Linux 下本就无效)统一 no-op。
  var noop = function () { return Promise.resolve(null); };
  var winHandlers = {
    'plugin:window|show': function () { return Promise.resolve(cur().show()); },
    'plugin:window|hide': function () { return Promise.resolve(cur().hide()); },
    'plugin:window|close': function () { return Promise.resolve(cur().close()); },
    'plugin:window|destroy': function () { return Promise.resolve(cur().close()); },
    'plugin:window|create': noop,           // 窗口创建走后端 open_oauth_window 命令
    'plugin:window|minimize': function () { return Promise.resolve(cur().minimize()); },
    'plugin:window|maximize': function () { return Promise.resolve(cur().maximize()); },
    'plugin:window|unminimize': function () { return Promise.resolve(cur().unminimize()); },
    'plugin:window|unmaximize': function () { return Promise.resolve(cur().unmaximize()); },
    'plugin:window|toggle_maximize': function () {
      return cur().isMaximized().then(function (m) { return m ? cur().unmaximize() : cur().maximize(); });
    },
    'plugin:window|is_maximized': function () { return cur().isMaximized(); },
    'plugin:window|is_minimized': function () { return cur().isMinimized(); },
    'plugin:window|is_visible': function () { return cur().isVisible(); },
    'plugin:window|is_decorated': function () { return cur().isDecorated(); },
    'plugin:window|is_resizable': function () { return cur().isResizable(); },
    'plugin:window|is_fullscreen': function () { return cur().isFullscreen(); },
    'plugin:window|is_focused': function () { return cur().isFocused(); },
    'plugin:window|is_maximizable': function () { return cur().isMaximizable(); },
    'plugin:window|is_minimizable': function () { return cur().isMinimizable(); },
    'plugin:window|is_closable': function () { return Promise.resolve(true); },
    'plugin:window|is_enabled': function () { return Promise.resolve(true); },
    'plugin:window|is_always_on_top': function () { return cur().isAlwaysOnTop(); },
    'plugin:window|title': function () { return cur().title(); },
    'plugin:window|theme': noop,
    'plugin:window|set_title': function (a) {
      var v = a.value !== undefined ? a.value : a.title;
      return Promise.resolve(cur().setTitle(v));
    },
    'plugin:window|set_size': function (a) {
      var s = v1Size(a, WIN.LogicalSize, WIN.PhysicalSize);
      if (s) cur().setSize(s);
      return Promise.resolve(null);
    },
    'plugin:window|set_min_size': function (a) {
      var s = v1Size(a, WIN.LogicalSize, WIN.PhysicalSize);
      if (s) cur().setMinSize(s);
      return Promise.resolve(null);
    },
    'plugin:window|set_max_size': function (a) {
      var s = v1Size(a, WIN.LogicalSize, WIN.PhysicalSize);
      if (s) cur().setMaxSize(s);
      return Promise.resolve(null);
    },
    'plugin:window|set_size_constraints': function (a) {
      // v2: {value:{minWidth,minHeight,maxWidth,maxHeight}}（可为 null）
      var c = a.value || a;
      if (c.minWidth != null && c.minHeight != null) {
        cur().setMinSize(new WIN.LogicalSize(c.minWidth, c.minHeight));
      }
      if (c.maxWidth != null && c.maxHeight != null) {
        cur().setMaxSize(new WIN.LogicalSize(c.maxWidth, c.maxHeight));
      }
      return Promise.resolve(null);
    },
    'plugin:window|set_position': function (a) {
      var p = v1Size(a, WIN.LogicalPosition, WIN.PhysicalPosition);
      if (p) cur().setPosition(p);
      return Promise.resolve(null);
    },
    'plugin:window|center': function () { return Promise.resolve(cur().center()); },
    'plugin:window|set_focus': function () { return Promise.resolve(cur().setFocus()); },
    'plugin:window|request_user_attention': function (a) {
      // v2: {value: {type:'Critical'|'Informational'} | null}
      // v1: null=不请求, 1=Critical, 2=Informational
      var v = a.value;
      var t = null;
      if (v && typeof v === 'object') t = v.type === 'Critical' ? 1 : 2;
      else if (a.userAttentionType) t = a.userAttentionType === 2 ? 2 : 1;
      return Promise.resolve(cur().requestUserAttention(t));
    },
    // v2 的布尔开关参数都在 value 上
    'plugin:window|set_fullscreen': function (a) {
      var v = a.value !== undefined ? a.value : a.fullscreen;
      return Promise.resolve(cur().setFullscreen(!!v));
    },
    'plugin:window|set_simple_fullscreen': noop,
    'plugin:window|set_resizable': function (a) {
      var v = a.value !== undefined ? a.value : a.resizable;
      return Promise.resolve(cur().setResizable(!!v));
    },
    'plugin:window|set_decorations': function (a) {
      var v = a.value !== undefined ? a.value : a.decorated;
      return Promise.resolve(cur().setDecorations(!!v));
    },
    'plugin:window|set_always_on_top': function (a) {
      var v = a.value !== undefined ? a.value : a.alwaysOnTop;
      return Promise.resolve(cur().setAlwaysOnTop(!!v));
    },
    'plugin:window|set_always_on_bottom': noop,
    'plugin:window|set_skip_taskbar': function (a) {
      var v = a.value !== undefined ? a.value : a.skipTaskbar;
      return Promise.resolve(cur().setSkipTaskbar(!!v));
    },
    'plugin:window|set_focusable': noop,
    'plugin:window|set_closable': noop,
    'plugin:window|set_maximizable': noop,   // v1 无对应（GTK 无法单独禁用最大化）
    'plugin:window|set_minimizable': noop,
    'plugin:window|set_enabled': noop,
    'plugin:window|set_icon': noop,
    'plugin:window|set_overlay_icon': noop,
    'plugin:window|set_badge_count': noop,
    'plugin:window|set_badge_label': noop,
    'plugin:window|set_progress_bar': noop,
    'plugin:window|set_shadow': noop,
    'plugin:window|set_title_bar_style': noop,   // macOS 专属
    'plugin:window|set_theme': noop,
    'plugin:window|set_visible_on_all_workspaces': noop,
    'plugin:window|set_background_color': noop,
    'plugin:window|set_effects': noop,           // 窗口材质效果,Linux 无对应
    'plugin:window|set_content_protected': noop,
    'plugin:window|set_cursor_grab': noop,
    'plugin:window|set_cursor_icon': noop,
    'plugin:window|set_cursor_position': noop,
    'plugin:window|set_cursor_visible': noop,
    'plugin:window|set_ignore_cursor_events': noop,
    'plugin:window|start_dragging': function () { return Promise.resolve(cur().startDragging()); },
    'plugin:window|start_resize_dragging': noop, // v1 无对应;GTK decorations:false 时边缘缩放由系统处理
    'plugin:window|inner_position': function () {
      return cur().innerPosition().then(function (p) { return { x: p.x, y: p.y }; });
    },
    'plugin:window|outer_position': function () {
      return cur().outerPosition().then(function (p) { return { x: p.x, y: p.y }; });
    },
    'plugin:window|inner_size': function () {
      return cur().innerSize().then(function (s) { return { width: s.width, height: s.height }; });
    },
    'plugin:window|outer_size': function () {
      return cur().outerSize().then(function (s) { return { width: s.width, height: s.height }; });
    },
    'plugin:window|scale_factor': function () { return cur().scaleFactor(); },
    'plugin:window|get_all_windows': function () {
      return Promise.resolve([{ label: (T.metadata && T.metadata.currentWindow && T.metadata.currentWindow.label) || 'main' }]);
    }
  };

  // ---- plugin:dialog|* (v1 dialog 模块) ----
  // v2 的 open/save 参数是 { options: {...} }（message 是扁平参数）；
  // 按扁平读会丢掉 directory:true，导致本应选目录的地方弹成选文件。
  function dlgOptions(a) { return (a && a.options) || a || {}; }
  var dlgHandlers = {
    'plugin:dialog|message': function (a) {
      return Promise.resolve(DLG.message(a.message, { title: a.title, type: a.kind }));
    },
    'plugin:dialog|open': function (a) {
      var o = dlgOptions(a);
      var opt = {
        title: o.title,
        multiple: o.multiple,
        directory: o.directory,
        defaultPath: o.defaultPath
      };
      // v2 filters: [{name, extensions}] -> v1 filters: {name, extensions}
      if (o.filters && o.filters.length) opt.filters = o.filters[0];
      return DLG.open(opt);
    },
    'plugin:dialog|save': function (a) {
      var o = dlgOptions(a);
      var opt = { title: o.title, defaultPath: o.defaultPath };
      if (o.filters && o.filters.length) opt.filters = o.filters[0];
      return DLG.save(opt);
    }
  };

  // ---- plugin:event|* (v1 event 模块 + transformCallback id) ----
  var evHandlers = {
    'plugin:event|listen': function (a) {
      var key = String(a.handler);
      var id = 'evt-' + (++seq);
      return EV.listen(a.event, function (e) {
        var cb = cbMap[key] || window['_' + a.handler];
        if (cb) cb(e);
      }).then(function (unlisten) {
        unlistenMap[id] = unlisten;
        return id;
      });
    },
    'plugin:event|unlisten': function (a) {
      var id = String(a.eventId != null ? a.eventId : a.id);
      var fn = unlistenMap[id];
      if (fn) { fn(); delete unlistenMap[id]; }
      return Promise.resolve(null);
    },
    'plugin:event|emit': function (a) { return EV.emit(a.event, a.payload); },
    // v1 无定向 emit;广播语义弱化(目标场景为单窗口应用)
    'plugin:event|emit_to': function (a) { return EV.emit(a.event, a.payload); }
  };

  // ---- plugin:opener|* (v1 shell 模块,系统 xdg-open) ----
  var openerHandlers = {
    'plugin:opener|open_url': function (a) { return Promise.resolve(SH.open(a.url)); },
    'plugin:opener|open_path': function (a) { return Promise.resolve(SH.open(a.path)); }
  };

  // ---- plugin:autostart|* -> commands::set_autostart_state/get_autostart_state ----
  // tauri-plugin-autostart 无 v1 适配版本；复用后端既有自启动命令（语义一致）。
  var autostartHandlers = {
    'plugin:autostart|enable': function () { return TAURI.invoke('set_autostart_state', { enabled: true }); },
    'plugin:autostart|disable': function () { return TAURI.invoke('set_autostart_state', { enabled: false }); },
    'plugin:autostart|is_enabled': function () { return TAURI.invoke('get_autostart_state'); }
  };

  // ---- 其余 v2 插件命令 ----
  // v1 没有对应实现、且 Linux 单窗口场景用不到的，统一 no-op，避免前端抛
  // “plugin xxx not found” 打断后续流程（例如设置页读版本、更新检查）。
  var miscHandlers = {
    'plugin:app|version': function () { return TAURI.invoke('get_app_version'); },
    'plugin:process|restart': function () { return TAURI.invoke('app_restart'); },
    'plugin:resources|close': noop,
    'plugin:webview|get_all_webviews': function () { return Promise.resolve([]); },
    'plugin:webview|set_webview_background_color': noop,
    'plugin:webview|set_webview_zoom': noop,
    'plugin:webview|clear_all_browsing_data': noop,
    'plugin:webview|webview_show': noop,
    'plugin:webview|webview_hide': noop,
    'plugin:webview|webview_close': noop,
    'plugin:image|from_bytes': noop,
    'plugin:image|from_path': noop,
    'plugin:image|new': noop,
    'plugin:image|rgba': noop,
    'plugin:image|size': noop,
    // 自动更新在界面上已禁用（服务端不提供分发），检查更新固定返回“无更新”
    'plugin:updater|check': function () { return Promise.resolve(null); }
  };

  var localHandlers = {};
  var k;
  for (k in winHandlers) localHandlers[k] = winHandlers[k];
  for (k in dlgHandlers) localHandlers[k] = dlgHandlers[k];
  for (k in evHandlers) localHandlers[k] = evHandlers[k];
  for (k in openerHandlers) localHandlers[k] = openerHandlers[k];
  for (k in autostartHandlers) localHandlers[k] = autostartHandlers[k];
  for (k in miscHandlers) localHandlers[k] = miscHandlers[k];

  // ---- 组装 v2 内部接口 ----
  window.__TAURI_INTERNALS__ = {
    invoke: function (cmd, args, options) {
      if (localHandlers[cmd]) return localHandlers[cmd](args || {});
      // plugin:autostart|* / plugin:mdns|* 由 Tauri1 插件按同名命令路由;业务命令直通
      return TAURI.invoke(cmd, args);
    },
    transformCallback: function (cb, once) {
      var id = ++cbId;
      cbMap[String(id)] = cb;
      window['_' + id] = cb;   // 保持 Tauri1 后端回调约定 window['_id']
      return id;
    },
    unregisterCallback: function (id) {
      delete cbMap[String(id)];
      delete window['_' + id];
    },
    convertFileSrc: function (path) { return TAURI.convertFileSrc ? TAURI.convertFileSrc(path) : path; },
    metadata: {
      currentWindow: (T.metadata && T.metadata.currentWindow) || { label: 'main' },
      // v1 无 webview 概念,补齐 v2 结构
      currentWebview: { label: (T.metadata && T.metadata.currentWindow && T.metadata.currentWindow.label) || 'main' }
    },
    plugins: {},
    __mdnsShim: true
  };

  // ---- v2 插件内部对象(官方前端 bundle 直接访问,不走 invoke) ----
  // plugin-os:同步 os 信息(platform/os_type/arch/family)
  var ua = navigator.userAgent || '';
  var archDetected = /aarch64|arm64/.test(ua) ? 'aarch64'
    : (/x86_64|x64|amd64/.test(ua) ? 'x86_64' : 'unknown');
  window.__TAURI_OS_PLUGIN_INTERNALS__ = {
    platform: 'linux',
    // 必须是小写：官方前端用 os_type 与 "linux"/"windows"/"macos" 比较
    //（大写 "Linux" 会导致侧栏 AppLogo 整块不渲染）。
    os_type: 'linux',
    arch: archDetected,
    family: 'unix',
    version: ''
  };
  // plugin-event:注销钩子;实际注销由我们的 plugin:event|unlisten 处理器完成
  window.__TAURI_EVENT_PLUGIN_INTERNALS__ = {
    unregisterListener: function (event, listenerId) {}
  };

  // ---- 页面异常收集(沿用原 v2 实现的 js_log 通道) ----
  var log = function (lvl, msg) {
    try { TAURI.invoke('js_log', { level: lvl, message: String(msg).slice(0, 600) }); } catch (e) { }
  };
  window.addEventListener('error', function (e) {
    log('js-error', (e.message || '') + ' @ ' + (e.filename || '') + ':' + (e.lineno || ''));
  });
  window.addEventListener('unhandledrejection', function (e) {
    log('js-reject', (e.reason && (e.reason.stack || e.reason)) || 'unknown');
  });
  log('boot', 'tauri1-shim installed, readyState=' + document.readyState);
})();
