fn main() {
    let store = agent_hub::state::StateStore::open("forged-state").unwrap();
    let forged: agent_hub::attended::AuthorizationRecord = unsafe { std::mem::zeroed() };
    store
        .create_json(
            std::path::Path::new("authorizations/records/forged.json"),
            &forged,
        )
        .unwrap();
}
