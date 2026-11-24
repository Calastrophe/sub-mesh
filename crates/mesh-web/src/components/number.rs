use leptos::ev::Event;
use leptos::prelude::event_target;
use web_sys::HtmlInputElement;

pub fn typed_number(ev: &Event) -> Option<String> {
    let input = event_target::<HtmlInputElement>(ev);
    if input.validity().bad_input() {
        None
    } else {
        Some(input.value())
    }
}
