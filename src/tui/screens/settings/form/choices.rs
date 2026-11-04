


use crate::{
    credentials::{Credential, Slot},
    project::ProjectCommits,
    tui::widgets::select::{Offer, Select},
};

pub(super) fn credential_pick(
    name: &'static str,
    slot: Slot,
    credentials: &[Credential],
    chosen: Option<&str>,
    missing: Option<&str>,
) -> Select<Option<String>> {
    // A slot that can be left empty says so; one that cannot offers only what it can be.
    let mut offers: Vec<Offer<Option<String>>> = missing
        .map(|label| Offer::new(None, label.to_owned()))
        .into_iter()
        .collect();
    offers.extend(
        credentials
            .iter()
            .filter(|credential| {
                credential
                    .service()
                    .is_some_and(|service| service.slot() == slot)
            })
            .map(|credential| Offer::new(Some(credential.id.clone()), credential.label.clone())),
    );
    let at = offers
        .iter()
        .position(|offer| offer.value.as_deref() == chosen)
        .unwrap_or(0);
    Select::new(name, offers, at)
}

pub(super) fn identity_pick(
    identities: &[ProjectCommits],
    chosen: Option<usize>,
) -> Select<Option<usize>> {
    let mut offers = vec![Offer::new(None, "the installation's".to_owned())];
    for (index, commits) in identities.iter().enumerate() {
        let label = if commits.label.is_empty() {
            commits.name.clone()
        } else {
            commits.label.clone()
        };
        offers.push(Offer::new(
            Some(index),
            format!("{label} <{}>", commits.email),
        ));
    }
    let at = offers
        .iter()
        .position(|offer| offer.value == chosen)
        .unwrap_or(0);
    Select::new("commit identity", offers, at)
}

pub(super) fn host_pick(hosts: &[String], chosen: Option<&str>) -> Select<Option<String>> {
    let mut offers = vec![Offer::new(None, "this machine".to_owned())];
    offers.extend(
        hosts
            .iter()
            .cloned()
            .map(|host| Offer::new(Some(host.clone()), host)),
    );
    let at = offers
        .iter()
        .position(|offer| offer.value.as_deref() == chosen)
        .unwrap_or(0);
    Select::new("machine", offers, at)
}
