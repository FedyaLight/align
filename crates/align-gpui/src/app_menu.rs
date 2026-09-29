//! Project the registered application menus into the in-window menu.

use gpui::{OwnedMenu, OwnedMenuItem};

use crate::{HideApp, HideOthersApp, ShowAllApps};

pub fn in_window_menus(menus: Vec<OwnedMenu>) -> Vec<OwnedMenu> {
    menus
        .into_iter()
        .filter_map(|mut menu| {
            if menu.name.as_ref() == "Window" {
                return None;
            }
            let mut items = Vec::new();
            for item in menu.items {
                let item = match item {
                    OwnedMenuItem::SystemMenu(_) => continue,
                    OwnedMenuItem::Action { ref action, .. }
                        if action.as_any().is::<HideApp>()
                            || action.as_any().is::<HideOthersApp>()
                            || action.as_any().is::<ShowAllApps>() =>
                    {
                        continue;
                    }
                    OwnedMenuItem::Submenu(submenu) => {
                        let Some(submenu) = in_window_menus(vec![submenu]).pop() else {
                            continue;
                        };
                        OwnedMenuItem::Submenu(submenu)
                    }
                    item => item,
                };
                if matches!(item, OwnedMenuItem::Separator)
                    && (items.is_empty() || matches!(items.last(), Some(OwnedMenuItem::Separator)))
                {
                    continue;
                }
                items.push(item);
            }
            if matches!(items.last(), Some(OwnedMenuItem::Separator)) {
                items.pop();
            }
            menu.items = items;
            (!menu.items.is_empty()).then_some(menu)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LightAppearance;
    use gpui::{Menu, MenuItem, SystemMenuType};

    #[test]
    fn registered_submenus_keep_their_actions_without_platform_only_items() {
        let menus = vec![
            Menu {
                name: "Custom".into(),
                items: vec![
                    MenuItem::separator(),
                    MenuItem::action("A renamed system command", HideApp),
                    MenuItem::os_submenu("Services", SystemMenuType::Services),
                    MenuItem::submenu(Menu {
                        name: "New settings group".into(),
                        items: vec![
                            MenuItem::action("Another system command", HideOthersApp),
                            MenuItem::separator(),
                            MenuItem::action("✓ Custom appearance", LightAppearance),
                            MenuItem::separator(),
                            MenuItem::action("Show applications", ShowAllApps),
                        ],
                    }),
                    MenuItem::separator(),
                ],
            }
            .owned(),
            Menu {
                name: "Window".into(),
                items: vec![MenuItem::action("Window command", LightAppearance)],
            }
            .owned(),
        ];

        let projected = in_window_menus(menus);
        assert_eq!(projected.len(), 1, "Window must not appear in the popup");
        let [OwnedMenuItem::Submenu(submenu)] = projected[0].items.as_slice() else {
            panic!("Only the custom settings group should remain");
        };
        assert_eq!(submenu.name.as_ref(), "New settings group");
        let [OwnedMenuItem::Action { name, action, .. }] = submenu.items.as_slice() else {
            panic!("System commands and orphan separators must be removed recursively");
        };
        assert_eq!(name, "✓ Custom appearance");
        assert!(action.as_any().is::<LightAppearance>());
    }
}
