use leptos::prelude::*;

/// Shared table layout with right-aligned numeric columns at the end. `class`
/// adds a modifier class to the table.
#[component]
pub fn Table(
    headers: &'static [&'static str],
    numeric_headers: &'static [&'static str],
    #[prop(optional)] class: &'static str,
    #[prop(optional)] footer: Option<Children>,
    children: Children,
) -> impl IntoView {
    view! {
        <div class="table-wrap">
            <table class=format!("table {class}")>
                <thead>
                    <tr>
                        {headers.iter().map(|&name| view! { <th>{name}</th> }).collect_view()}
                        {numeric_headers.iter().map(|&name| view! { <th class="num">{name}</th> }).collect_view()}
                    </tr>
                </thead>
                <tbody>{children()}</tbody>
            </table>
            {footer.map(|footer| footer())}
        </div>
    }
}
