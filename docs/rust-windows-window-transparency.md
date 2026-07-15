# Rust + Windows : windows transparentes qui marchent partout

Notes issues d'une session dédiée à faire fonctionner des fenêtres transparentes en Rust sur Windows 11 ARM64 dans Parallels Desktop. Toutes les trouvailles ci-dessous ont été validées sur : Windows 11 ARM64 (Parallels 26 sur Mac Apple Silicon), avec le stack Rust classique `eframe` / `egui` / `tao` / `wry`. Applicable aussi à Windows natif x64/ARM64 (l'inverse est plus simple — beaucoup de bugs Parallels sont absents sur bare metal).

## TL;DR — la matrice qui marche

| Cas d'usage | Stack recommandé | Config critique |
|---|---|---|
| **Widget egui semi-transparent + coins arrondis** | `eframe` (feature `wgpu`, PAS `glow`) + `egui` | `ViewportBuilder::with_transparent(true) + with_decorations(false)`. Peindre un panel arrondi via `ui.painter().rect_filled(rect, radius, color)`. |
| **Widget egui opaque + coins arrondis** | `eframe` (feature au choix) + `egui` | Pas de `with_transparent`. Peindre un panel via `Frame::new().corner_radius(N)` OU `painter.rect_filled` avec radius. |
| **Popup HTML fullscreen transparent (Lottie / animation)** | `tao` + `wry` (WebView2) | `WindowBuilder::with_transparent(true) + with_undecorated_shadow(false)` avant build, puis `window.set_undecorated_shadow(true)` après. `WebViewBuilder::with_transparent(true)`. HTML `body { background: transparent }`. |
| **Overlay/notification always-on-top** | idem widget/popup + `apply_topmost_toolwindow` post-create | Win32 `SetWindowLongPtrW(GWL_EXSTYLE, current \| WS_EX_TOOLWINDOW \| WS_EX_TOPMOST)` + `SetWindowPos(HWND_TOPMOST, ...)`. Retire d'Alt+Tab et de la taskbar. |
| **Fenêtre transparente sur Parallels ARM64** | **impérativement `eframe/wgpu`** (pas glow) OU tao/wry | Voir § « Le piège ANGLE ». Le stack OpenGL virtualisé refuse alpha framebuffer. |

## Le stack graphique Windows en 30 secondes

Comprendre les couches est indispensable pour diagnostiquer un bug de transparence :

```
  ┌──────────────────────────────────────────────────────────────┐
  │  Application Rust                                            │
  │  ├── eframe (glow → OpenGL) / eframe (wgpu → D3D12)          │
  │  ├── tao (winit fork) → Win32 CreateWindowExW                │
  │  ├── wry → WebView2 (Edge Chromium) → DirectComposition      │
  │  └── raw Win32 GDI (UpdateLayeredWindow)                     │
  ├──────────────────────────────────────────────────────────────┤
  │  Windows OS                                                  │
  │  ├── DWM (Desktop Window Manager) — compositor               │
  │  │    DwmEnableBlurBehindWindow, DwmSetWindowAttribute       │
  │  │    LWA_ALPHA / LWA_COLORKEY, per-pixel alpha              │
  │  ├── DXGI / DirectComposition — GPU compositing              │
  │  └── ANGLE (Google) — OpenGL → D3D11 translation layer       │
  └──────────────────────────────────────────────────────────────┘
                              │
  ┌──────────────────────────────────────────────────────────────┐
  │  Driver / GPU                                                │
  │  ├── Bare metal : Intel / AMD / NVIDIA / Qualcomm            │
  │  └── Virtualisé : Parallels (D3D11/12 → Metal via ANGLE      │
  │      pour OpenGL, D3D natif OK)                              │
  └──────────────────────────────────────────────────────────────┘
```

Points clés :

- **DWM** = compositor Windows depuis Vista. Il assemble toutes les windows en une seule image écran, gère l'alpha compositing per-pixel, les ombres, les effets acrylic.
- **DXGI / DirectComposition** = APIs Microsoft pour donner des surfaces alpha aux apps GPU-accelerated. Utilisées par WebView2 (Chromium).
- **ANGLE** = **traducteur OpenGL → D3D11** de Google. Utilisé par défaut par Chrome, glutin (Rust EGL binding), et beaucoup de crates OpenGL sur Windows.
- Sur **Parallels**, ANGLE tourne mais son adapter EGL refuse certaines configs (notamment alpha pixel format) sur le driver graphique virtualisé.

## Le piège ANGLE (le vrai coupable Parallels ARM64)

`eframe` avec le backend par défaut `glow` utilise :

```
eframe → egui-glow → glow (OpenGL binding) → glutin (EGL context creator) → ANGLE → D3D11 → GPU
```

Quand on demande `ViewportBuilder::with_transparent(true)`, glutin appelle `eglChooseConfig` avec `EGL_ALPHA_SIZE = 8`. Sur Parallels ARM64, ANGLE retourne **zéro configs matching**. Résultat :

```
ERROR Exiting because of error: Found no glutin configs matching the template:
ConfigTemplate { color_buffer_type: Rgb { r_size: 8, g_size: 8, b_size: 8 },
  alpha_size: 8, depth_size: 0, stencil_size: 0, ...,
  transparency: true, ... }.
Error: [0] L'opération a réussi. (os error 0)
```

Le message « L'opération a réussi » alors qu'on a une erreur est particulièrement trompeur — c'est le comportement ANGLE quand aucun config ne matche mais qu'aucun appel Win32 sous-jacent n'a échoué.

**Fix** : passer eframe sur backend `wgpu` :

```toml
[dependencies]
eframe = { version = "0.35", default-features = false, features = ["default_fonts", "wgpu"] }
```

Wgpu parle **directement à D3D12** sur Windows, sans intermédiaire ANGLE. Il expose des surfaces alpha natives (`SurfaceConfiguration.alpha_mode = PostMultiplied`). Sur Parallels, D3D12 est virtualisé natively sans traducteur = ça juste marche.

Coût : binaire +2-3 MB (wgpu-hal est plus gros que glow-hal), compile time +30-50%, mais aucune régression fonctionnelle. Requis Windows 10 1607+ (2016), i.e. toutes les machines des 10 dernières années.

## Les 3 couches de transparence WebView2

Pour un popup HTML transparent (`tao` + `wry`), il faut **3 layers** correctement configurés :

### 1. Window layer (tao)

```rust
let builder = WindowBuilder::new()
    .with_decorations(false)
    .with_transparent(true)
    // ↓ CRITICAL Windows-specific dance ↓
    .with_undecorated_shadow(false);   // avant build
let window = builder.build(&event_loop)?;
window.set_undecorated_shadow(true);   // après build
```

Le toggle `false → true` sur `undecorated_shadow` est **impératif sur Windows**. Sans lui, le DWM shadow attribute set à la création collide avec la surface DirectComposition de WebView2 → DWM tombe en fallback opaque → **fond blanc**.

Le pattern vient directement de [wry/examples/transparent.rs](https://github.com/tauri-apps/wry/blob/dev/examples/transparent.rs) — la référence officielle. Tao's `with_transparent(true)` sur Windows implémente la transparence via `DwmEnableBlurBehindWindow` avec une région vide (le pattern DWM classique pour per-pixel alpha).

### 2. WebView layer (wry)

```rust
let webview = WebViewBuilder::new()
    .with_transparent(true)
    .with_url("purrpause://localhost/popup.html")
    .build(&window)?;
```

En interne, wry appelle `ICoreWebView2Controller2.SetDefaultBackgroundColor(COREWEBVIEW2_COLOR { R: 0, G: 0, B: 0, A: 0 })` sur le controller WebView2. Sans ce call, WebView2 remplit sa surface avec du blanc opaque par défaut.

### 3. HTML layer

```html
<body style="background: transparent;">
  <!-- ton contenu ici -->
</body>
```

Le body du HTML doit avoir un background transparent. Sinon le rendu final est opaque même si les 2 couches en dessous supportent l'alpha.

**Les 3 couches sont indépendantes.** Manquer une seule = fond opaque final. Ordre de composition : desktop → tao window (avec DWM blur-behind) → WebView2 surface (DirectComposition) → HTML body → contenu.

## L'astuce `undecorated_shadow` en détail

`with_undecorated_shadow(false)` désactive le drop shadow que Windows ajoute par défaut aux fenêtres non-décorées. Documentation winit :

> `with_undecorated_shadow` — Shows or hides the background drop shadow for undecorated windows.
> The shadow is hidden by default.
> **Enabling the shadow causes a thin 1px line to appear on the top of the window.**

Ce liseré 1px c'est exactement l'artefact qu'on veut éviter. Mais alors pourquoi le dance `false → true` marche pour WebView2 ?

Réponse : quand on demande `with_transparent(true) + set_undecorated_shadow(true)`, tao/wry appellent en interne le pattern DWM correct qui INITIALIZE la surface DWM avec l'alpha channel activé DÈS le début, avant que WebView2 ne s'y attache. Sans ce toggle, WebView2 s'attache à une surface DWM par défaut (opaque) et ne peut pas la réinitialiser après coup.

C'est un **quirk d'ordre d'initialisation** dans DWM. La solution `false → true` c'est un « reset then re-enable » qui force DWM à faire une passe fraîche.

## Le trick WS_EX_LAYERED (fallback ultime)

Si tu ne peux pas utiliser eframe/wgpu OU tao/wry (par ex. tu construis ta propre window Win32 GDI), tu peux forcer une transparence uniforme via WS_EX_LAYERED :

```rust
use windows::Win32::UI::WindowsAndMessaging::{
    SetLayeredWindowAttributes, SetWindowLongPtrW,
    GetWindowLongPtrW, GWL_EXSTYLE,
    WS_EX_LAYERED, LWA_ALPHA,
};

unsafe {
    let cur = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
    SetWindowLongPtrW(hwnd, GWL_EXSTYLE, cur | WS_EX_LAYERED.0 as isize);
    SetLayeredWindowAttributes(
        hwnd,
        windows::Win32::Foundation::COLORREF(0),
        217,        // alpha 0-255 ; 217 ≈ 85% opaque
        LWA_ALPHA,
    )?;
}
```

Avantages :
- Marche sur **n'importe quel GPU backend** (OpenGL, D3D9, D3D11, GDI).
- Pas de config alpha framebuffer requise du driver.
- API Win32 de Windows 2000, présente partout.

Limitations :
- **Uniforme sur toute la window** (pas de per-pixel alpha). Une region ne peut pas être 100% opaque et une autre 50%.
- Coins arrondis « transparents autour d'un rectangle opaque » impossibles.

Gotcha : winit (donc eframe et tao) **réapplique** ses propres attributs sur chaque redraw. Une fois-tir sur `SetLayeredWindowAttributes` sera écrasé quelques frames plus tard. Fix : appeler dans chaque frame de `App::ui` (cheap syscall).

Alternative complexe : `UpdateLayeredWindow` avec un bitmap BGRA depuis un memory DC = per-pixel alpha SANS GPU. ~300 lignes de GDI, pas d'egui derrière (faut redraw le contenu manuellement).

## Autres attributes DWM utiles

### `DWMWA_NCRENDERING_POLICY = DWMNCRP_DISABLED`

Désactive le rendering non-client (title bar chrome, drop shadow, resize handles). Utile pour un widget vraiment borderless.

```rust
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMNCRP_DISABLED, DWMWA_NCRENDERING_POLICY,
};
unsafe {
    let policy = DWMNCRP_DISABLED;
    DwmSetWindowAttribute(hwnd, DWMWA_NCRENDERING_POLICY, &policy as *const _ as *const _, 4)?;
}
```

**⚠️ Attention** : cet attribute désactive **toutes** les DWM effects sur cette window, y compris `DwmEnableBlurBehindWindow`. Si tu l'utilises sur une window tao/wry transparente = **tu tues la transparence**. À réserver aux windows opaques.

### `DWMWA_WINDOW_CORNER_PREFERENCE = DWMWCP_DONOTROUND`

Windows 11 arrondit automatiquement les coins de toutes les top-level windows avec un subtle liseré. Pour l'éviter :

```rust
use windows::Win32::Graphics::Dwm::{DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND};
unsafe {
    let corner = DWMWCP_DONOTROUND;
    let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &corner as *const _ as *const _, 4);
}
```

Safe no-op sur Windows 10 (API 20H1+).

### `DwmEnableBlurBehindWindow` avec région vide

Le pattern classique pour per-pixel alpha via DWM :

```rust
use windows::Win32::Graphics::Dwm::{DwmEnableBlurBehindWindow, DWM_BB_ENABLE, DWM_BB_BLURREGION, DWM_BLURBEHIND};
use windows::Win32::Graphics::Gdi::CreateRectRgn;

unsafe {
    let region = CreateRectRgn(0, 0, -1, -1); // région vide
    let bb = DWM_BLURBEHIND {
        dwFlags: DWM_BB_ENABLE | DWM_BB_BLURREGION,
        fEnable: true.into(),
        hRgnBlur: region,
        fTransitionOnMaximized: false.into(),
    };
    DwmEnableBlurBehindWindow(hwnd, &bb)?;
    // libérer region après appel
}
```

C'est exactement ce que fait tao's `with_transparent(true)` en interne. Rarement utile de l'appeler manuellement.

## DPI awareness (le piège Retina/Parallels)

Sur écran high-DPI (Retina, Parallels avec scaling), `GetMonitorInfoW` retourne des **pixels physiques** (ex. 2646×1558), mais eframe/winit `ViewportBuilder::with_position` prend des **logical points**.

Si tu passes `(2306, 20)` en pensant que c'est physical → winit l'interprète comme logical → position physique effective = `(2306 × scale, 20 × scale)` = `(4612, 40)` sur un écran scale=2.0 → **window off-screen**, invisible mais présente dans la taskbar.

Fix : query le scale factor et diviser :

```rust
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

let mut dpix = 96u32;
let mut dpiy = 96u32;
unsafe { GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dpix, &mut dpiy)?; }
let scale = dpix as f32 / 96.0;

let x_logical = physical_x as f32 / scale;
let y_logical = physical_y as f32 / scale;

let options = eframe::NativeOptions {
    viewport: ViewportBuilder::default()
        .with_position([x_logical, y_logical])
        ...
};
```

Requiert `Win32_UI_HiDpi` dans les features du windows crate.

Ton exe doit aussi être **DPI-aware** via le manifest UAC :

```xml
<application xmlns="urn:schemas-microsoft-com:asm.v3">
  <windowsSettings>
    <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">
      PerMonitorV2
    </dpiAwareness>
  </windowsSettings>
</application>
```

Sinon Windows ment sur les valeurs retournées par `GetMonitorInfoW` (il fait du "DPI virtualization" et te renvoie des valeurs déjà scaled pour te tromper de manière cohérente).

## Élévation UAC + spawn en session user (bonus)

Contexte hors transparence mais souvent rencontré ensemble : ton exe a un manifest `requireAdministrator`, tu tournes en LocalSystem (service Windows), et tu veux spawn un enfant dans la session interactive de l'user.

`CreateProcessAsUserW` échoue avec `ERROR_ELEVATION_REQUIRED` (0x800702E4) parce que le token retourné par `WTSQueryUserToken` est le **filtered token** (medium integrity) — insuffisant pour lancer un exe qui demande l'élévation.

**Fix : query le linked token** :

```rust
use windows::Win32::Security::{
    GetTokenInformation, TokenLinkedToken, TOKEN_LINKED_TOKEN,
};

let mut info = TOKEN_LINKED_TOKEN::default();
let mut ret_len = 0u32;
let result = unsafe {
    GetTokenInformation(
        user_token,
        TokenLinkedToken,
        Some(&mut info as *mut _ as *mut _),
        std::mem::size_of::<TOKEN_LINKED_TOKEN>() as u32,
        &mut ret_len,
    )
};

let elevation_source = match result {
    Ok(()) if !info.LinkedToken.is_invalid() => info.LinkedToken,
    _ => user_token,  // pas de linked → l'user n'est pas un split-token admin
};
// Utilise elevation_source dans DuplicateTokenEx puis CreateProcessAsUserW.
```

Les admins Windows ont un modèle « split token » : deux tokens (medium + high integrity). `WTSQueryUserToken` retourne toujours le medium. Le linked token est le high — celui capable de lancer un exe `requireAdministrator`.

## Pattern « sandbox » pour diagnostiquer

Quand une combinaison de flags ne marche pas et qu'on ne sait pas pourquoi, la meilleure approche = un **mode sandbox** dans le même exe qui teste chaque flag en isolation :

```rust
// argv : --sandbox --try <preset>
match preset {
    "plain" => { /* opaque, décoré, no styling */ }
    "transparent" => { /* + with_transparent(true) */ }
    "borderless" => { /* + with_decorations(false) */ }
    "borderless-transparent" => { /* les deux */ }
    "topmost" => { /* + always_on_top */ }
    "fullscreen" => { /* + Fullscreen::Borderless */ }
    // etc.
}
```

Pour chaque preset, rendre un carré rouge avec une croix noire (test de visibilité binaire : rouge = window rendered, pas rouge = échec). Loguer les params + exit code + first-frame reached. Un script `.bat` séquentiel enchaîne les presets, l'utilisateur note X/O pour chacun.

Cette approche a permis d'isoler « transparency:true KO sur Parallels » vs « transparency:false OK » en 15 minutes.

Structure sandbox exemple : voir `apps/purrpause/src/modes/sandbox.rs`.

## Gotchas rencontrés (chronologique)

Bonus utile pour futur debug — les fausses pistes qu'on a suivies :

- **`SetLayeredWindowAttributes(LWA_ALPHA)` one-shot** = clobbered par winit à chaque frame. Fix : re-apply dans chaque `App::ui`.
- **`disable_dwm_nc_rendering(DWMNCRP_DISABLED)` sur widget transparent** = tue la transparence tao (car tao utilise DwmEnableBlurBehindWindow qui est aussi une DWM effect). À utiliser uniquement sur windows opaques.
- **`with_fullscreen(Some(Fullscreen::Borderless(None)))` + transparency** = NE cause PAS de Fullscreen Exclusive Mode automatique sur Windows (théorie fausse). Marche bien avec transparency SI le dance `undecorated_shadow` est appliqué.
- **`ChangeServiceConfigW` API** = pas exposée par `windows-service` 0.8.1. Fallback = `delete + register`.
- **`GetLastError` after successful call** peut retourner l'erreur de l'appel précédent. Toujours check le return value du call en cours, pas GetLastError seul.
- **`FileVersion` vide dans VERSIONINFO** = normal si tu ne set pas explicitement le manifest RC. Cosmétique.
- **Cargo.lock stale** avec `windows` crate 0.62 + wgpu-hal 29 = pas de conflit versions. Le premier build fail était juste un cache lockfile pas cohérent, `cargo update` a réparé.

## Cheat sheet dispatch

Décision rapide selon ton use-case :

```
├── Tu veux du HTML/CSS/JS (Lottie, animations complexes, layout riche) ?
│   → Popup fullscreen transparent : tao + wry + undecorated_shadow dance
│
├── Tu veux du GUI natif Rust (widget, panneaux, contrôles) ?
│   ├── Transparent per-pixel avec coins ronds sur desktop ?
│   │   → eframe + feature wgpu + ViewportBuilder::with_transparent(true)
│   │
│   ├── Opaque avec rounded corners aesthetic ?
│   │   → eframe + n'importe quel backend, panel avec Frame::corner_radius(N)
│   │
│   └── Overlay always-on-top (notification, alert) ?
│       → + apply_topmost_toolwindow (WS_EX_TOOLWINDOW | WS_EX_TOPMOST)
│
└── Tu construis une window Win32 GDI custom ?
    → WS_EX_LAYERED + UpdateLayeredWindow (per-pixel) OU SetLayeredWindowAttributes (uniform)
```

## Bibliographie

- [wry/examples/transparent.rs](https://github.com/tauri-apps/wry/blob/dev/examples/transparent.rs) — l'exemple qui donne la solution.
- [Microsoft Docs — DwmSetWindowAttribute](https://learn.microsoft.com/en-us/windows/win32/api/dwmapi/nf-dwmapi-dwmsetwindowattribute).
- [Microsoft Docs — SetLayeredWindowAttributes](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setlayeredwindowattributes).
- [Microsoft Docs — UpdateLayeredWindow](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-updatelayeredwindow).
- [winit docs — `WindowAttributesExtWindows`](https://docs.rs/winit/latest/x86_64-pc-windows-msvc/winit/platform/windows/trait.WindowAttributesExtWindows.html).
- [ANGLE project](https://github.com/google/angle) — le traducteur OpenGL→D3D à l'origine du problème glow.
- [WebView2 CoreWebView2Controller2](https://learn.microsoft.com/en-us/microsoft-edge/webview2/reference/win32/icorewebview2controller2) — la couche « bg color transparent ».

## Historique de la session ayant produit ces notes

- **v0.1.0 → 0.1.9** : essais successifs avec eframe/glow. Widget invisible sur Parallels (bug ANGLE alpha framebuffer), popup blanc, plein de fausses pistes.
- **v0.2.0-0.2.1** : fallback LWA_ALPHA (uniforme). Marche mais limitations (pas de per-pixel).
- **v0.3.0** : **switch eframe glow → wgpu**. Débloque tout côté widget.
- **v0.3.1-0.3.4** : mode `--sandbox --try <preset>` pour diagnostiquer via matrix. 14 presets, isole précisément quelle combo passe.
- **v0.4.0-0.4.1** : restauration transparence prod widget + popup. Widget OK, popup toujours blanc.
- **v0.4.2** : trouvé le dance `with_undecorated_shadow(false) → set_undecorated_shadow(true)` dans wry example → **popup transparent OK**.
- **v0.4.3** : simplification, widget en classic rounding opaque, service passe `--no-debug` en prod.

Temps total : ~une session de smoke intensif (10-15 versions livrées).
