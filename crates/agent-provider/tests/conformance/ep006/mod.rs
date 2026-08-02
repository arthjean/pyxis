mod compact;
mod files;
mod images;
mod memories;
mod realtime;
mod search;
mod support;

use super::AuxiliaryCase;

pub(super) async fn assert_auxiliary_fixture(name: &str, case: &AuxiliaryCase) {
    match case.family.as_str() {
        "compact" => compact::assert_fixture(name, case).await,
        "memories" => memories::assert_fixture(name, case).await,
        "images" => images::assert_fixture(name, case).await,
        "search" => search::assert_fixture(name, case).await,
        "files" => files::assert_fixture(name, case).await,
        "realtime" => realtime::assert_fixture(name, case).await,
        family => panic!("unknown EP-006 auxiliary family: {family}"),
    }
}
