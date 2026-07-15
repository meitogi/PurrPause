// WebView2 runtime pre-flight — bail early with a native MessageBoxW
// dialog if the WebView2 runtime is missing. Popup rendering (session 4)
// requires it ; there's no point starting the service without it.

use anyhow::{anyhow, Result};
use webview2_com::Microsoft::Web::WebView2::Win32::GetAvailableCoreWebView2BrowserVersionString;
use windows::core::{HSTRING, PCWSTR, PWSTR};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, IDOK, MB_ICONWARNING, MB_OKCANCEL, SW_SHOWNORMAL,
};

const WEBVIEW2_DOWNLOAD_URL: &str =
    "https://developer.microsoft.com/en-us/microsoft-edge/webview2/";

pub fn ensure_available() -> Result<()> {
    let mut version_out = PWSTR::null();
    let hr = unsafe {
        GetAvailableCoreWebView2BrowserVersionString(PCWSTR::null(), &mut version_out)
    };
    match hr {
        Ok(()) if !version_out.is_null() => {
            tracing::info!("WebView2 runtime detected");
            // The docs say the caller must CoTaskMemFree the returned string.
            unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(version_out.0 as _)) };
            Ok(())
        }
        _ => {
            show_missing_dialog();
            Err(anyhow!("WebView2 runtime absent — user notified"))
        }
    }
}

fn show_missing_dialog() {
    let title: HSTRING = "Composant manquant".into();
    let body: HSTRING = "Windows Session Health Service nécessite le runtime Microsoft Edge WebView2, absent de ce système.\n\n\
         WebView2 est préinstallé sur Windows 11 et Windows 10 (2020+). Il peut être installé gratuitement depuis Microsoft.\n\n\
         [OK] ouvre la page de téléchargement.  [Annuler] quitte."
        .into();

    let choice = unsafe {
        MessageBoxW(
            None,
            &body,
            &title,
            MB_OKCANCEL | MB_ICONWARNING,
        )
    };

    if choice == IDOK {
        let url: HSTRING = WEBVIEW2_DOWNLOAD_URL.into();
        let verb: HSTRING = "open".into();
        unsafe {
            ShellExecuteW(
                None,
                &verb,
                &url,
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            );
        }
    }
}
