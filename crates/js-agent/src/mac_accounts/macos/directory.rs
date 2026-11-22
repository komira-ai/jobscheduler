//! User records through OpenDirectory's local node.

use std::io;

use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2_foundation::{NSArray, NSError, NSString};
use objc2_open_directory::{
    ODNode, ODQuery, ODRecord, ODSession, kODMatchBeginsWith, kODMatchEqualTo,
    kODNodeTypeLocalNodes,
};

use super::super::home::account_attributes;
use super::super::host::NewUser;

const USERS: &str = "dsRecTypeStandard:Users";
/// The attribute that names a record.
const RECORD_NAME: &str = "dsAttrTypeStandard:RecordName";
/// The attribute that holds a user's uid.
const UNIQUE_ID: &str = "dsAttrTypeStandard:UniqueID";
const GROUPS: &str = "dsRecTypeStandard:Groups";
/// The group whose members are administrators.
const ADMIN_GROUP: &str = "admin";

/// Makes the record of `user`: its attributes are [`account_attributes`], and no password,
/// so the record has no `ShadowHash` in its authentication authority.
pub(super) fn create(user: &NewUser) -> io::Result<()> {
    autoreleasepool(|_| {
        let node = local_node()?;
        let mut error: Option<Retained<NSError>> = None;
        // SAFETY: the record type and name are NSStrings; no attributes yet.
        let record = unsafe {
            node.createRecordWithRecordType_name_attributes_error(
                Some(&NSString::from_str(USERS)),
                Some(&NSString::from_str(&user.name)),
                None,
                Some(&mut error),
            )
        }
        .ok_or_else(|| od_error("creating the record", error))?;
        for (attribute, value) in account_attributes(user) {
            set(&record, attribute, &value)?;
        }
        Ok(())
    })
}

/// Deletes the record `name`, and its membership of the `admin` group; whether there was
/// one. A removal from the group that fails is not judged here: the deletion is judged by
/// its outcome, and the account by the proof that follows it.
pub(super) fn delete(name: &str) -> io::Result<bool> {
    autoreleasepool(|_| {
        let node = local_node()?;
        let Some(record) = find(&node, name)? else {
            return Ok(false);
        };
        // A stale membership would make the next holder of the name an administrator, so
        // the group is cleaned first.
        let admin = group(&node, ADMIN_GROUP)?;
        let mut ignored = None;
        // SAFETY: both are records of the local node. Not a member is an error that changes
        // nothing.
        unsafe { admin.removeMemberRecord_error(Some(&record), Some(&mut ignored)) };
        let mut error = None;
        // SAFETY: a record of the local node.
        if !unsafe { record.deleteRecordAndReturnError(Some(&mut error)) } {
            return Err(od_error("deleting the record", error));
        }
        Ok(true)
    })
}

/// The records whose name starts with `prefix`, each with its uid. A record whose name or
/// uid cannot be read (one a crash left half made, between its creation and its uid) is
/// named in the log and passed over, so it does not stop the listing of the rest: with no
/// uid it holds none.
pub(super) fn list(prefix: &str) -> io::Result<Vec<(String, u32)>> {
    autoreleasepool(|_| {
        let node = local_node()?;
        let kinds = NSString::from_str(USERS);
        let kinds: &AnyObject = &kinds;
        let attribute = NSString::from_str(RECORD_NAME);
        let prefix = NSString::from_str(prefix);
        let prefix: &AnyObject = &prefix;
        let wanted = [
            NSString::from_str(RECORD_NAME),
            NSString::from_str(UNIQUE_ID),
        ];
        let wanted = NSArray::from_slice(&[&*wanted[0], &*wanted[1]]);
        let wanted: &AnyObject = &wanted;
        let mut error = None;
        // SAFETY: the node, the record type, the attribute, the value and the list of
        // attributes to return are live objects of the types the query takes; no limit on
        // the number of results (0).
        let query = unsafe {
            ODQuery::queryWithNode_forRecordTypes_attribute_matchType_queryValues_returnAttributes_maximumResults_error(
                Some(&node),
                Some(kinds),
                Some(&attribute),
                kODMatchBeginsWith,
                Some(prefix),
                Some(wanted),
                0,
                Some(&mut error),
            )
        }
        .ok_or_else(|| od_error("querying the accounts", error))?;
        let mut error = None;
        // SAFETY: a query just made; all its results, none partial.
        let results = unsafe { query.resultsAllowingPartial_error(false, Some(&mut error)) }
            .ok_or_else(|| od_error("listing the accounts", error))?;
        let mut accounts = Vec::new();
        for index in 0..results.count() {
            let item = results.objectAtIndex(index);
            let record = item
                .downcast_ref::<ODRecord>()
                .ok_or_else(|| io::Error::other("a result of the query is not a record"))?;
            let name = match first_value(record, RECORD_NAME) {
                Ok(name) => name,
                Err(why) => {
                    tracing::warn!("an account record without a readable name: {why}");
                    continue;
                }
            };
            let uid = match first_value(record, UNIQUE_ID).and_then(|uid| {
                uid.parse().map_err(|_| {
                    io::Error::other(format!("the uid of {name} is not a number: {uid}"))
                })
            }) {
                Ok(uid) => uid,
                Err(why) => {
                    tracing::warn!("passing over {name}: {why}");
                    continue;
                }
            };
            accounts.push((name, uid));
        }
        Ok(accounts)
    })
}

/// Whether the user record `name` exists. Unlike `getpwnam_r`, the directory reports a
/// failed lookup as an error, so an error here is never read as an absence.
pub(super) fn exists(name: &str) -> io::Result<bool> {
    autoreleasepool(|_| {
        let node = local_node()?;
        find(&node, name).map(|record| record.is_some())
    })
}

/// Whether a user record has the uid `uid`: a query of the local node for `UniqueID` equal
/// to it. A failure of the query or of reading its results is an error, not an absence.
pub(super) fn uid_exists(uid: u32) -> io::Result<bool> {
    autoreleasepool(|_| {
        let node = local_node()?;
        let kinds = NSString::from_str(USERS);
        let kinds: &AnyObject = &kinds;
        let attribute = NSString::from_str(UNIQUE_ID);
        let value = NSString::from_str(&uid.to_string());
        let value: &AnyObject = &value;
        let mut error = None;
        // SAFETY: the node, the record type, the attribute and the value are live objects of
        // the types the query takes; no attributes to return, and at most one result.
        let query = unsafe {
            ODQuery::queryWithNode_forRecordTypes_attribute_matchType_queryValues_returnAttributes_maximumResults_error(
                Some(&node),
                Some(kinds),
                Some(&attribute),
                kODMatchEqualTo,
                Some(value),
                None,
                1,
                Some(&mut error),
            )
        }
        .ok_or_else(|| od_error("querying the uid", error))?;
        let mut error = None;
        // SAFETY: a query just made; all its results, none partial.
        let results = unsafe { query.resultsAllowingPartial_error(false, Some(&mut error)) }
            .ok_or_else(|| od_error("listing the records of a uid", error))?;
        Ok(results.count() > 0)
    })
}

/// The local directory node (`/Local/Default`).
fn local_node() -> io::Result<Retained<ODNode>> {
    // SAFETY: plain constructors; the session may be the default (nil).
    let session = unsafe { ODSession::defaultSession() };
    let mut error = None;
    // SAFETY: as above.
    unsafe {
        ODNode::nodeWithSession_type_error(
            session.as_deref(),
            kODNodeTypeLocalNodes,
            Some(&mut error),
        )
    }
    .ok_or_else(|| od_error("opening the local node", error))
}

/// The group record `name` of `node`.
fn group(node: &ODNode, name: &str) -> io::Result<Retained<ODRecord>> {
    let mut error = None;
    // SAFETY: the record type and name are NSStrings; no attributes needed.
    unsafe {
        node.recordWithRecordType_name_attributes_error(
            Some(&NSString::from_str(GROUPS)),
            Some(&NSString::from_str(name)),
            None,
            Some(&mut error),
        )
    }
    .ok_or_else(|| od_error(&format!("looking up group {name}"), error))
}

/// The user record `name` of `node`; `None` when there is none.
fn find(node: &ODNode, name: &str) -> io::Result<Option<Retained<ODRecord>>> {
    let mut error = None;
    // SAFETY: the record type and name are NSStrings; no attributes needed.
    let record = unsafe {
        node.recordWithRecordType_name_attributes_error(
            Some(&NSString::from_str(USERS)),
            Some(&NSString::from_str(name)),
            None,
            Some(&mut error),
        )
    };
    match (record, error) {
        (Some(record), _) => Ok(Some(record)),
        (None, None) => Ok(None),
        (None, error) => Err(od_error("looking the user up", error)),
    }
}

/// The first value of `attribute` of `record`, as text.
fn first_value(record: &ODRecord, attribute: &str) -> io::Result<String> {
    let mut error = None;
    // SAFETY: the attribute name is an NSString.
    let values = unsafe {
        record.valuesForAttribute_error(Some(&NSString::from_str(attribute)), Some(&mut error))
    }
    .ok_or_else(|| od_error(&format!("reading {attribute}"), error))?;
    let first = values
        .firstObject()
        .ok_or_else(|| io::Error::other(format!("{attribute} has no value")))?;
    first
        .downcast_ref::<NSString>()
        .map(ToString::to_string)
        .ok_or_else(|| io::Error::other(format!("{attribute} is not text")))
}

/// Sets the single value of `attribute` on `record`.
fn set(record: &ODRecord, attribute: &str, value: &str) -> io::Result<()> {
    let value = NSString::from_str(value);
    let value: &AnyObject = &value;
    let mut error = None;
    // SAFETY: the value is an NSString and the attribute name an NSString.
    if unsafe {
        record.setValue_forAttribute_error(
            Some(value),
            Some(&NSString::from_str(attribute)),
            Some(&mut error),
        )
    } {
        return Ok(());
    }
    Err(od_error(&format!("setting {attribute}"), error))
}

fn od_error(what: &str, error: Option<Retained<NSError>>) -> io::Error {
    let why = error.map_or_else(
        || "no error given".to_owned(),
        |error| error.localizedDescription().to_string(),
    );
    io::Error::other(format!("OpenDirectory, {what}: {why}"))
}
