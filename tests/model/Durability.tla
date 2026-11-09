---------------------------- MODULE Durability ----------------------------
EXTENDS Naturals, Integers, TLC

\* One existing local object, two bounded overwrite sessions, no media loss.
\* data is a symbolic whole-file content; -1 represents a torn buffered write.
\* Ordinary WRITE/CLOSE may lose data. FLUSH guarantees the current content and
\* metadata only until another overwrite starts: there is no historical copy.
\* A crash discards volatile state but retains fsynced data and synchronous WAL.
\* Namespace publication/directory fsync are covered by PhysicalRename instead.
\*
\* Implementation: core/file.rs::open_id synchronously invalidates protected
\* metadata before mutating a body; write_at updates metadata without syncing;
\* FileHandle::sync calls sync_object before core/store.rs::sync_metadata.
\* The lock is held through FLUSH, so writes cannot interleave its stages.
\* This does not model fsync/WAL errors (which must not return success), actual
\* sector tearing, device fsync dishonesty, checksums or recovery availability.
CONSTANTS MaxGeneration, CheckInvalidation, CheckDataSync, CheckMetadataSync
VARIABLES metadata, durableMetadata, data, durableData, phase, unsynced, acknowledged
vars == <<metadata, durableMetadata, data, durableData, phase, unsynced, acknowledged>>
None == -1
Initial == [generation |-> 0, protected |-> TRUE, content |-> 0]

Init ==
    /\ metadata = Initial /\ durableMetadata = Initial
    /\ data = 0 /\ durableData = 0 /\ phase = "idle"
    /\ unsynced = FALSE /\ acknowledged = None

BeginWrite ==
    /\ phase = "idle" /\ metadata.generation < MaxGeneration
    /\ metadata' = [metadata EXCEPT !.generation = @ + 1, !.protected = FALSE]
    /\ durableMetadata' = IF CheckInvalidation /\ durableMetadata.protected
                          THEN metadata' ELSE durableMetadata
    /\ phase' = "ready" /\ acknowledged' = None
    /\ UNCHANGED <<data, durableData, unsynced>>

Write ==
    /\ phase = "ready"
    /\ data' = metadata.generation
    /\ metadata' = [metadata EXCEPT !.content = metadata.generation]
    /\ unsynced' = TRUE /\ phase' = "written"
    /\ UNCHANGED <<durableMetadata, durableData, acknowledged>>

\* Unrequested writeback can persist all or part of unsynced data. WAL can also
\* persist before the data: only dirty records may describe such content.
DataWriteback ==
    /\ unsynced /\ durableData' \in {None, data}
    /\ UNCHANGED <<metadata, durableMetadata, data, phase, unsynced, acknowledged>>
MetadataWriteback ==
    /\ durableMetadata' = metadata
    /\ UNCHANGED <<metadata, data, durableData, phase, unsynced, acknowledged>>

FlushData ==
    /\ phase = "written" /\ phase' = "data-synced"
    /\ durableData' = IF CheckDataSync THEN data ELSE durableData
    /\ unsynced' = IF CheckDataSync THEN FALSE ELSE unsynced
    /\ UNCHANGED <<metadata, durableMetadata, data, acknowledged>>
FlushMetadata ==
    /\ phase = "data-synced" /\ phase' = "metadata-synced"
    /\ durableMetadata' = IF CheckMetadataSync THEN metadata ELSE durableMetadata
    /\ UNCHANGED <<metadata, data, durableData, unsynced, acknowledged>>
Acknowledge ==
    /\ phase = "metadata-synced" /\ phase' = "acknowledged"
    /\ acknowledged' = data
    /\ UNCHANGED <<metadata, durableMetadata, data, durableData, unsynced>>
Close ==
    /\ phase \in {"ready", "written", "acknowledged"} /\ phase' = "idle"
    /\ UNCHANGED <<metadata, durableMetadata, data, durableData, unsynced, acknowledged>>
Crash ==
    /\ metadata' = durableMetadata /\ data' = durableData
    /\ phase' = "idle" /\ unsynced' = FALSE
    /\ UNCHANGED <<durableMetadata, durableData, acknowledged>>

Next == BeginWrite \/ Write \/ DataWriteback \/ MetadataWriteback
        \/ FlushData \/ FlushMetadata \/ Acknowledge \/ Close \/ Crash
MetadataType == [generation : 0..MaxGeneration, protected : BOOLEAN,
                 content : 0..MaxGeneration]
TypeOK ==
    /\ metadata \in MetadataType /\ durableMetadata \in MetadataType
    /\ data \in None..MaxGeneration /\ durableData \in None..MaxGeneration
    /\ phase \in {"idle", "ready", "written", "data-synced",
                  "metadata-synced", "acknowledged"}
    /\ unsynced \in BOOLEAN /\ acknowledged \in None..MaxGeneration
ProtectedContentUnchanged ==
    durableMetadata.protected => durableData = durableMetadata.content
AcknowledgedDurable ==
    acknowledged # None =>
        /\ durableData = acknowledged
        /\ durableMetadata.content = acknowledged
Spec == Init /\ [][Next]_vars
=============================================================================
