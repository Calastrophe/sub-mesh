use leptos::prelude::*;

use crate::format::display_label;
use crate::state::use_store;

#[component]
pub fn ServerCard() -> impl IntoView {
    let store = use_store();
    let items = move || {
        let show = store.show_emails.get();
        store.settings.with(|view| {
            let fixed = &view.as_ref()?.fixed;
            let admins = if fixed.admins.is_empty() {
                "—".to_owned()
            } else {
                fixed
                    .admins
                    .iter()
                    .map(|login| display_label(login, show))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let proxy = fixed.trusted_proxy.as_ref().map_or_else(
                || "off".to_owned(),
                |proxy| {
                    let mut text = format!("{} · {}", proxy.addrs.join(", "), proxy.header);
                    if proxy.only {
                        text.push_str(" · only");
                    }
                    text
                },
            );
            let items = [
                ("Listen", fixed.listen.clone()),
                ("Admins", admins),
                ("Trusted proxy", proxy),
                ("Database", fixed.db_path.clone()),
                (
                    "Web dir",
                    fixed.web_dir.clone().unwrap_or_else(|| "—".to_owned()),
                ),
                ("Tailscale socket", fixed.tailscale_socket.clone()),
                (
                    "Skip whois",
                    if fixed.insecure_skip_whois {
                        "on"
                    } else {
                        "off"
                    }
                    .to_owned(),
                ),
            ];
            Some(
                items
                    .into_iter()
                    .map(|(name, value)| {
                        view! {
                            <div class="kv">
                                <dt>{name}</dt>
                                <dd class="mono">{value}</dd>
                            </div>
                        }
                    })
                    .collect_view(),
            )
        })
    };
    view! {
        <section class="group">
            <div class="group-title">
                "Server" <span class="badge">"server.toml"</span>
            </div>
            <dl class="kv-grid server-grid">{items}</dl>
        </section>
    }
}
