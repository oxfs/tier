-------------------------- MODULE PhysicalRename --------------------------
EXTENDS Naturals, FiniteSets, TLC

\* One existing object, three physical names (UUID + old/new filename hint).
\* All hard links reference the SAME mutable inode, not historical versions.
\* Protocol: sync candidate-GC intent, link(new), fsync(directory), sync DB
\* mapping + old-name GC + candidate-intent deletion in one transaction, then
\* unlink(old) and fsync(directory). Crash restores durable DB and names;
\* every non-current durable alias remains tracked by a GC job or intent.
\* File locks cover intent through publication and each GC decision/unlink.
\* An old alias can be reused before GC (A -> B -> A): existing names must
\* first be checked for the same dev/ino. All names here denote that inode.
\* Intent and old-name garbage are separate even if they name the same alias:
\* publishing cancels its candidate intent, not an older queued ABA GC job.
\* Candidate jobs for the same alias collapse to one set member: retiring them
\* together at publication is safe because that alias becomes the live mapping.
\* The model checks these boundaries, not Rust lock implementation or liveness.
\* It assumes successful filesystem calls and honest fsync/WAL, same filesystem
\* hard links, atomic RocksDB transactions, and no concurrent physical unlink
\* outside GC. GC drops a stale job without unlinking its current mapping.
\* Directory cleanup must never recursively remove an object directory.
CONSTANTS Names, CheckIntent, CheckLinkSync, CheckMappingSync, CheckGCCurrent
ASSUME Cardinality(Names) = 3
VARIABLES names, durableNames, mapping, durableMapping, garbage, durableGarbage,
          intents, durableIntents, phase, destination, content
vars == <<names, durableNames, mapping, durableMapping, garbage, durableGarbage,
          intents, durableIntents, phase, destination, content>>

Init ==
    /\ \E n \in Names :
        /\ names = {n} /\ durableNames = {n}
        /\ mapping = n /\ durableMapping = n /\ destination = n
    /\ garbage = {} /\ durableGarbage = {}
    /\ intents = {} /\ durableIntents = {}
    /\ phase = "idle" /\ content = 0

Plan(n) ==
    /\ phase = "idle" /\ n # mapping
    /\ destination' = n /\ phase' = "planned"
    /\ intents' = IF CheckIntent THEN intents \cup {n} ELSE intents
    \* A synchronous WAL transaction also persists preceding metadata writes.
    /\ durableIntents' = intents'
    /\ durableMapping' = mapping /\ durableGarbage' = garbage
    /\ UNCHANGED <<names, durableNames, mapping, garbage, content>>
Link ==
    /\ phase = "planned"
    /\ names' = names \cup {destination} /\ phase' = "linked"
    /\ UNCHANGED <<durableNames, mapping, durableMapping, garbage, durableGarbage,
                   intents, durableIntents, destination, content>>
SyncLink ==
    /\ phase = "linked" /\ phase' = "link-synced"
    /\ durableNames' = IF CheckLinkSync THEN names ELSE durableNames
    /\ UNCHANGED <<names, mapping, durableMapping, garbage, durableGarbage,
                   intents, durableIntents, destination, content>>
Publish ==
    /\ phase = "link-synced"
    /\ mapping' = destination /\ garbage' = garbage \cup {mapping}
    /\ intents' = intents \ {destination}
    /\ durableMapping' = IF CheckMappingSync THEN mapping' ELSE durableMapping
    /\ durableGarbage' = IF CheckMappingSync THEN garbage' ELSE durableGarbage
    /\ durableIntents' = IF CheckMappingSync THEN intents' ELSE durableIntents
    /\ phase' = "idle"
    /\ UNCHANGED <<names, durableNames, destination, content>>

\* GC holds the same file lock. It can observe an ABA-reused name in its queue:
\* in that case consume the job but leave the current body's hard link intact.
\* A directory-sync-before-GC-deletion crash leaves a harmless retryable job;
\* those two steps are collapsed since extra jobs do not affect these invariants.
Collect(n) ==
    /\ phase = "idle" /\ (n \in garbage \/ n \in intents)
    /\ names' = IF CheckGCCurrent /\ n = mapping THEN names ELSE names \ {n}
    /\ durableNames' = IF CheckGCCurrent /\ n = mapping
                       THEN durableNames ELSE durableNames \ {n}
    /\ garbage' = garbage \ {n} /\ durableGarbage' = durableGarbage \ {n}
    /\ intents' = intents \ {n}
    /\ durableIntents' = durableIntents \ {n}
    /\ UNCHANGED <<mapping, durableMapping, phase, destination, content>>
DataWrite ==
    /\ content' = 1 - content
    /\ UNCHANGED <<names, durableNames, mapping, durableMapping, garbage,
                   durableGarbage, intents, durableIntents, phase, destination>>
DirectoryWriteback ==
    /\ durableNames' = names
    /\ UNCHANGED <<names, mapping, durableMapping, garbage, durableGarbage,
                   intents, durableIntents, phase, destination, content>>
MetadataWriteback ==
    /\ durableMapping' = mapping /\ durableGarbage' = garbage /\ durableIntents' = intents
    /\ UNCHANGED <<names, durableNames, mapping, garbage, intents, phase, destination, content>>
Crash ==
    /\ names' = durableNames /\ mapping' = durableMapping /\ garbage' = durableGarbage
    /\ intents' = durableIntents /\ phase' = "idle" /\ destination' = durableMapping
    /\ UNCHANGED <<durableNames, durableMapping, durableGarbage, durableIntents, content>>

Next == (\E n \in Names : Plan(n) \/ Collect(n))
        \/ Link \/ SyncLink \/ Publish \/ DataWrite
        \/ DirectoryWriteback \/ MetadataWriteback \/ Crash
TypeOK ==
    /\ names \subseteq Names /\ durableNames \subseteq Names
    /\ mapping \in Names /\ durableMapping \in Names /\ destination \in Names
    /\ garbage \subseteq Names /\ durableGarbage \subseteq Names
    /\ intents \subseteq Names /\ durableIntents \subseteq Names
    /\ phase \in {"idle", "planned", "linked", "link-synced"} /\ content \in {0, 1}
DurableMappingHasBody == durableMapping \in durableNames
LiveMappingHasBody == mapping \in names
NoUntrackedAlias ==
    durableNames \ {durableMapping} \subseteq durableGarbage \cup durableIntents
Spec == Init /\ [][Next]_vars
=============================================================================
