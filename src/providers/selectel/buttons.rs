// Selectel account management callback constants
pub const CB_SELECTEL_ACCOUNTS: &str = "adm_sel_accts";
pub const CB_SELECTEL_ADD_ACCOUNT: &str = "adm_sel_add";
pub const CB_SELECTEL_DETAIL_PREFIX: &str = "adm_sel:";
pub const CB_SELECTEL_DELETE_PREFIX: &str = "adm_seldel:";
pub const CB_SELECTEL_ACTIVATE_PREFIX: &str = "adm_selact:";
pub const CB_SELECTEL_DEACTIVATE_DB_PREFIX: &str = "adm_seldeadb:";
pub const CB_SELECTEL_DEACTIVATE_PROVIDER_PREFIX: &str = "adm_seldeapv:";
// Hard-delete the (already-deactivated) account row. Suffix is acct_id.
pub const CB_SELECTEL_REMOVE_PREFIX: &str = "adm_selrm:";
pub const CB_SELECTEL_REGION_PREFIX: &str = "adm_selrgn:";
// Region change: chgrgn = open picker (followed by acct_id);
//                chgrgnp = pick (followed by "{acct_id}:{region}");
//                chgrgnc = confirm + delete server (followed by "{acct_id}:{region}").
pub const CB_SELECTEL_CHANGE_REGION_PREFIX: &str = "adm_sel_chgrgn:";
pub const CB_SELECTEL_CHGRGN_PICK_PREFIX: &str = "adm_sel_chgrgnp:";
pub const CB_SELECTEL_CHGRGN_CONFIRM_PREFIX: &str = "adm_sel_chgrgnc:";
// Multi-region failover priority list:
//   mr   = manage regions list (followed by acct_id)
//   mra  = add-region picker (followed by acct_id)
//   mrap = apply add (followed by "{acct_id}:{region}")
//   mrrm = remove region (followed by "{acct_id}:{region}")
pub const CB_SELECTEL_MR_LIST_PREFIX: &str = "adm_sel_mr:";
pub const CB_SELECTEL_MR_ADD_PREFIX: &str = "adm_sel_mra:";
pub const CB_SELECTEL_MR_ADD_PICK_PREFIX: &str = "adm_sel_mrap:";
pub const CB_SELECTEL_MR_REMOVE_PREFIX: &str = "adm_sel_mrrm:";
