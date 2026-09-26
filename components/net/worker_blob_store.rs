/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! In-memory blob store for the Cloudflare Worker build.
//!
//! The native file manager (`filemanager_thread.rs`) needs threads, tokio and
//! a filesystem, none of which a Worker has. Script still sends it the same
//! messages to create, slice, read, reference-count and revoke blobs, and some
//! callers block on the reply, so an unanswered message traps the instance.
//! This store answers them in process with the same bookkeeping as the native
//! `FileManagerStore`, for memory-backed and sliced blobs only: a Worker page
//! has no local files.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use net_traits::blob_url_store::{BlobBuf, BlobURLStoreError, UrlWithBlobClaim, parse_blob_url};
use net_traits::CoreResourceMsg;
use net_traits::filemanager_thread::{
    FileManagerThreadError, FileManagerThreadMsg, FileTokenCheck, GetTokenForFileReply,
    ReadFileProgress, RelativePos,
};
use servo_base::generic_channel::GenericSender;
use servo_url::ImmutableOrigin;
use uuid::Uuid;

enum BlobData {
    Memory(BlobBuf),
    /// A slice of the parent entry's data.
    Sliced(Uuid, RelativePos),
}

struct BlobEntry {
    /// Origin of the entry's creator.
    origin: ImmutableOrigin,
    data: BlobData,
    /// Script-side holders of this ID, plus sliced entries pointing at it.
    refs: usize,
    /// Whether the ID is currently a valid blob URL (createObjectURL).
    is_valid_url: bool,
    /// Tokens held by fetches that resolved the URL while it was valid.
    outstanding_tokens: HashSet<Uuid>,
}

thread_local! {
    static BLOBS: RefCell<HashMap<Uuid, BlobEntry>> = RefCell::new(HashMap::new());
    /// Sender into the Worker resource channel, handed to token holders so
    /// they can refresh and revoke their tokens.
    static RESOURCE_SENDER: RefCell<Option<GenericSender<CoreResourceMsg>>> =
        const { RefCell::new(None) };
}

pub(crate) fn set_resource_sender(sender: GenericSender<CoreResourceMsg>) {
    RESOURCE_SENDER.with(|slot| *slot.borrow_mut() = Some(sender));
}

pub(crate) fn handle(message: FileManagerThreadMsg) {
    match message {
        FileManagerThreadMsg::ReadFile(sender, id, origin) => {
            match read(&id, &FileTokenCheck::NotRequired, &origin, RelativePos::full_range()) {
                Ok(buf) => {
                    let _ = sender.send(Ok(ReadFileProgress::Meta(buf)));
                    let _ = sender.send(Ok(ReadFileProgress::EOF));
                },
                Err(error) => {
                    let _ = sender.send(Err(FileManagerThreadError::BlobURLStoreError(error)));
                },
            }
        },
        FileManagerThreadMsg::PromoteMemory(id, blob_buf, set_valid, origin) => {
            insert(id, origin, BlobData::Memory(blob_buf), set_valid);
        },
        FileManagerThreadMsg::AddSlicedURLEntry(parent_id, rel_pos, sender, origin) => {
            let result = inc_ref(&parent_id, &origin).map(|()| {
                let id = Uuid::new_v4();
                // Valid: AddSlicedURLEntry implies createObjectURL on a slice.
                insert(id, origin, BlobData::Sliced(parent_id, rel_pos), true);
                id
            });
            let _ = sender.send(result);
        },
        FileManagerThreadMsg::DecRef(id, origin, sender) => {
            let _ = sender.send(dec_ref(&id, &origin));
        },
        FileManagerThreadMsg::ActivateBlobURL(id, sender, origin) => {
            let _ = sender.send(set_blob_url_validity(true, &id, &origin));
        },
        FileManagerThreadMsg::RevokeBlobURL(id, origin, sender) => {
            let _ = sender.send(set_blob_url_validity(false, &id, &origin));
        },
        FileManagerThreadMsg::GetTokenForFile(id, sender) => {
            let Some(resource_sender) = RESOURCE_SENDER.with(|slot| slot.borrow().clone()) else {
                return;
            };
            let token = match token_for_file(&id, false) {
                FileTokenCheck::Required(token) => Some(token),
                _ => None,
            };
            let _ = sender.send(GetTokenForFileReply {
                token,
                revoke_sender: resource_sender.clone(),
                refresh_sender: resource_sender,
            });
        },
        FileManagerThreadMsg::RevokeTokenForFile(token, id) => {
            invalidate_token(&FileTokenCheck::Required(token), &id);
        },
        // There is no file picker; dropping the callback leaves the input
        // element without a selection.
        FileManagerThreadMsg::SelectFiles(..) => {},
    }
}

/// Resolve the bytes a `blob:` URL refers to, for a fetch of that URL. As in
/// the native blob protocol handler, the URL's claim token (acquired when the
/// request was created, while the URL was valid) identifies the blob and its
/// origin; without one, the URL must still be valid when it is fetched.
pub fn read_blob_url(url: &UrlWithBlobClaim) -> Result<BlobBuf, BlobURLStoreError> {
    if let Some(token) = url.token() {
        return read(
            &token.file_id,
            &FileTokenCheck::Required(token.token),
            &token.origin,
            RelativePos::full_range(),
        );
    }
    let id = parse_blob_url(&url.url()).map_err(|_| BlobURLStoreError::InvalidFileID)?;
    let check = token_for_file(&id, false);
    let result = read(&id, &check, &url.url().origin(), RelativePos::full_range());
    invalidate_token(&check, &id);
    result
}

pub(crate) fn token_for_file(id: &Uuid, allow_revoked: bool) -> FileTokenCheck {
    BLOBS.with(|blobs| {
        let mut blobs = blobs.borrow_mut();
        // Validity belongs to the URL's own entry: a slice's URL is valid
        // while its parent, promoted only to back the slice, is not.
        let (holder, valid) = match blobs.get(id) {
            Some(BlobEntry {
                data: BlobData::Sliced(parent_id, _),
                is_valid_url,
                ..
            }) => (*parent_id, *is_valid_url),
            Some(entry) => (*id, entry.is_valid_url),
            None => return FileTokenCheck::ShouldFail,
        };
        if !allow_revoked && !valid {
            return FileTokenCheck::ShouldFail;
        }
        // The token lives on the entry holding the data, keeping it alive.
        let Some(entry) = blobs.get_mut(&holder) else {
            return FileTokenCheck::ShouldFail;
        };
        let token = Uuid::new_v4();
        entry.outstanding_tokens.insert(token);
        FileTokenCheck::Required(token)
    })
}

/// The entry that holds tokens for `id`: its parent for a slice.
fn token_holder(id: &Uuid) -> Uuid {
    BLOBS.with(|blobs| match blobs.borrow().get(id) {
        Some(BlobEntry {
            data: BlobData::Sliced(parent_id, _),
            ..
        }) => *parent_id,
        _ => *id,
    })
}

pub(crate) fn invalidate_token(token: &FileTokenCheck, id: &Uuid) {
    let FileTokenCheck::Required(token) = token else {
        return;
    };
    let id = token_holder(id);
    BLOBS.with(|blobs| {
        let mut blobs = blobs.borrow_mut();
        let Some(entry) = blobs.get_mut(&id) else {
            return;
        };
        entry.outstanding_tokens.remove(token);
        if entry.refs == 0 && entry.outstanding_tokens.is_empty() && !entry.is_valid_url {
            blobs.remove(&id);
        }
    });
}

fn insert(id: Uuid, origin: ImmutableOrigin, data: BlobData, is_valid_url: bool) {
    BLOBS.with(|blobs| {
        blobs.borrow_mut().insert(
            id,
            BlobEntry {
                origin,
                data,
                refs: 1,
                is_valid_url,
                outstanding_tokens: HashSet::new(),
            },
        )
    });
}

fn read(
    id: &Uuid,
    token: &FileTokenCheck,
    origin: &ImmutableOrigin,
    rel_pos: RelativePos,
) -> Result<BlobBuf, BlobURLStoreError> {
    BLOBS.with(|blobs| {
        let blobs = blobs.borrow();
        let mut id = *id;
        let mut rel_pos = rel_pos;
        loop {
            let entry = blobs.get(&id).ok_or(BlobURLStoreError::InvalidFileID)?;
            if entry.origin != *origin {
                return Err(BlobURLStoreError::InvalidOrigin);
            }
            match token {
                FileTokenCheck::NotRequired => {},
                FileTokenCheck::Required(token) if entry.outstanding_tokens.contains(token) => {},
                // A sliced entry's token lives on its parent.
                FileTokenCheck::Required(_) if matches!(entry.data, BlobData::Sliced(..)) => {},
                _ => return Err(BlobURLStoreError::InvalidFileID),
            }
            match &entry.data {
                BlobData::Memory(buf) => {
                    let range = rel_pos.to_abs_range(buf.size as usize);
                    let bytes = buf
                        .bytes
                        .get(range.clone())
                        .ok_or(BlobURLStoreError::InvalidRange)?;
                    return Ok(BlobBuf {
                        filename: None,
                        type_string: buf.type_string.clone(),
                        size: range.len() as u64,
                        bytes: bytes.to_vec(),
                    });
                },
                BlobData::Sliced(parent_id, inner_rel_pos) => {
                    rel_pos = rel_pos.slice_inner(inner_rel_pos);
                    id = *parent_id;
                },
            }
        }
    })
}

fn inc_ref(id: &Uuid, origin: &ImmutableOrigin) -> Result<(), BlobURLStoreError> {
    BLOBS.with(|blobs| match blobs.borrow_mut().get_mut(id) {
        Some(entry) if entry.origin == *origin => {
            entry.refs += 1;
            Ok(())
        },
        Some(_) => Err(BlobURLStoreError::InvalidOrigin),
        None => Err(BlobURLStoreError::InvalidFileID),
    })
}

fn dec_ref(id: &Uuid, origin: &ImmutableOrigin) -> Result<(), BlobURLStoreError> {
    let parent = BLOBS.with(|blobs| {
        let mut blobs = blobs.borrow_mut();
        let entry = blobs.get_mut(id).ok_or(BlobURLStoreError::InvalidFileID)?;
        if entry.origin != *origin {
            return Err(BlobURLStoreError::InvalidOrigin);
        }
        entry.refs = entry.refs.saturating_sub(1);
        if entry.refs > 0 || entry.is_valid_url || !entry.outstanding_tokens.is_empty() {
            return Ok(None);
        }
        let parent = match entry.data {
            BlobData::Sliced(parent_id, _) => Some(parent_id),
            BlobData::Memory(_) => None,
        };
        blobs.remove(id);
        Ok(parent)
    })?;
    match parent {
        Some(parent_id) => dec_ref(&parent_id, origin),
        None => Ok(()),
    }
}

fn set_blob_url_validity(
    validity: bool,
    id: &Uuid,
    origin: &ImmutableOrigin,
) -> Result<(), BlobURLStoreError> {
    let parent = BLOBS.with(|blobs| {
        let mut blobs = blobs.borrow_mut();
        let entry = blobs.get_mut(id).ok_or(BlobURLStoreError::InvalidFileID)?;
        if entry.origin != *origin {
            return Err(BlobURLStoreError::InvalidOrigin);
        }
        entry.is_valid_url = validity;
        if validity || entry.refs > 0 || !entry.outstanding_tokens.is_empty() {
            return Ok(None);
        }
        let parent = match entry.data {
            BlobData::Sliced(parent_id, _) => Some(parent_id),
            BlobData::Memory(_) => None,
        };
        blobs.remove(id);
        Ok(parent)
    })?;
    match parent {
        Some(parent_id) => dec_ref(&parent_id, origin),
        None => Ok(()),
    }
}
