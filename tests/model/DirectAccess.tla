--------------------------- MODULE DirectAccess ---------------------------
EXTENDS Naturals, TLC

\* One representative session and object, two bounded mutations. A grant reply
\* can be delayed past revocation; ACK can be delayed past lease expiry or lost
\* on loss of both sockets. Disconnect does not kill the daemon or expire the
\* client's grant: the registry must retain that deadline independently of sockets.
\* All sessions must satisfy this protocol before a mutation.
\* Local operation is its result-publication boundary: deadline and daemon
\* identity must still be valid there. OS scheduling, clock precision, inode
\* identity checks, memory safety, and liveness are outside this finite model.
\* An opened fd holds a kernel pin independently of mapping leases. Background
\* publication/GC skips pinned files; foreground writes keep the same inode.
\* Waiting transitions hold no storage-worker resource in this abstraction;
\* actual Tokio/tevent scheduling must additionally be covered by integration tests.
CONSTANTS MaxEpoch, CheckPublication, CheckEpoch, CheckPins,
          CheckDeadline, CheckDaemon, CheckDisconnect
VARIABLES epoch, generation, phase, daemon, connected, expired,
          pending, cached, floor, notified, ack, ackEpoch, handle,
          readsCurrent, handlesProtected, grantsOutsideMutation
vars == <<epoch, generation, phase, daemon, connected, expired,
          pending, cached, floor, notified, ack, ackEpoch, handle,
          readsCurrent, handlesProtected, grantsOutsideMutation>>
Empty == [present |-> FALSE, epoch |-> 0, generation |-> 0, daemon |-> 0]
Usable == cached.present /\ ~expired
          /\ (~CheckDaemon \/ cached.daemon = daemon)

Init ==
    /\ epoch = 0 /\ generation = 0 /\ phase = "idle" /\ daemon = 0
    /\ connected = TRUE /\ expired = FALSE
    /\ pending = Empty /\ cached = Empty /\ floor = 0
    /\ notified = FALSE /\ ack = FALSE /\ ackEpoch = 0 /\ handle = "none"
    /\ readsCurrent = TRUE /\ handlesProtected = TRUE
    /\ grantsOutsideMutation = TRUE

Issue ==
    /\ connected /\ ~expired /\ ~pending.present
    /\ (~CheckPublication \/ phase = "idle")
    /\ pending' = [present |-> TRUE, epoch |-> epoch,
                    generation |-> generation, daemon |-> daemon]
    /\ grantsOutsideMutation' = (grantsOutsideMutation /\ phase = "idle")
    /\ UNCHANGED <<epoch, generation, phase, daemon, connected, expired,
                   cached, floor, notified, ack, ackEpoch, handle, readsCurrent, handlesProtected>>

DeliverGrant ==
    /\ connected /\ pending.present
    /\ cached' = IF ~CheckEpoch \/ pending.epoch >= floor THEN pending ELSE cached
    /\ pending' = Empty
    /\ UNCHANGED <<epoch, generation, phase, daemon, connected, expired,
                   floor, notified, ack, ackEpoch, handle, readsCurrent, handlesProtected,
                   grantsOutsideMutation>>

LocalOpen ==
    /\ Usable /\ handle = "none"
    /\ handle' = "local"
    /\ readsCurrent' = (readsCurrent /\ cached.generation = generation)
    /\ UNCHANGED <<epoch, generation, phase, daemon, connected, expired,
                   pending, cached, floor, notified, ack, ackEpoch, handlesProtected,
                   grantsOutsideMutation>>
LocalStatOrRead ==
    /\ Usable
    /\ readsCurrent' = (readsCurrent /\ cached.generation = generation)
    /\ UNCHANGED <<epoch, generation, phase, daemon, connected, expired,
                   pending, cached, floor, notified, ack, ackEpoch, handle, handlesProtected,
                   grantsOutsideMutation>>
Close ==
    /\ handle # "none" /\ handle' = "none"
    /\ UNCHANGED <<epoch, generation, phase, daemon, connected, expired,
                   pending, cached, floor, notified, ack, ackEpoch, readsCurrent,
                   handlesProtected, grantsOutsideMutation>>

BeginMutation ==
    /\ phase = "idle" /\ epoch < MaxEpoch
    /\ phase' = "revoke" /\ epoch' = epoch + 1
    /\ notified' = TRUE
    /\ UNCHANGED <<ack, ackEpoch>>
    /\ UNCHANGED <<generation, daemon, connected, expired, pending, cached,
                   floor, handle, readsCurrent, handlesProtected, grantsOutsideMutation>>

ReceiveRevoke ==
    /\ connected /\ notified
    /\ cached' = Empty /\ floor' = epoch /\ ack' = TRUE /\ notified' = FALSE
    /\ ackEpoch' = epoch
    /\ UNCHANGED <<epoch, generation, phase, daemon, connected, expired,
                   pending, handle, readsCurrent, handlesProtected, grantsOutsideMutation>>
ReceiveAck ==
    /\ connected /\ ack
    /\ phase' = IF phase = "revoke" /\ ackEpoch = epoch THEN "ready" ELSE phase
    /\ UNCHANGED handle
    /\ ack' = FALSE /\ UNCHANGED ackEpoch
    /\ UNCHANGED <<epoch, generation, daemon, connected, expired, pending,
                   cached, floor, notified, readsCurrent, handlesProtected,
                   grantsOutsideMutation>>

Expire ==
    /\ ~expired /\ expired' = TRUE
    /\ UNCHANGED <<epoch, generation, phase, daemon, connected, pending,
                   cached, floor, notified, ack, ackEpoch, handle, readsCurrent,
                   handlesProtected, grantsOutsideMutation>>
Timeout ==
    /\ phase = "revoke"
    /\ (~CheckDeadline \/ expired \/ (~CheckDisconnect /\ ~connected))
    /\ phase' = "ready"
    /\ UNCHANGED <<epoch, generation, daemon, connected, expired, pending,
                   cached, floor, notified, ack, ackEpoch, handle, readsCurrent,
                   handlesProtected, grantsOutsideMutation>>
Mutate ==
    /\ phase = "ready" /\ generation < MaxEpoch
    /\ (~CheckPins \/ handle = "none")
    /\ generation' = generation + 1 /\ phase' = "idle"
    /\ handlesProtected' = (handlesProtected /\ handle = "none")
    /\ UNCHANGED <<epoch, daemon, connected, expired, pending, cached,
                   floor, notified, ack, ackEpoch, handle, readsCurrent, grantsOutsideMutation>>
SkipBusy ==
    /\ phase = "ready" /\ handle = "local" /\ CheckPins
    /\ phase' = "idle"
    /\ UNCHANGED <<epoch, generation, daemon, connected, expired, pending, cached,
                   floor, notified, ack, ackEpoch, handle, readsCurrent,
                   handlesProtected, grantsOutsideMutation>>
Disconnect ==
    /\ connected /\ connected' = FALSE
    /\ UNCHANGED <<epoch, generation, phase, daemon, expired, pending,
                   cached, floor, notified, ack, ackEpoch, handle, readsCurrent,
                   handlesProtected, grantsOutsideMutation>>
Restart ==
    /\ daemon = 0 /\ generation < MaxEpoch
    \* A new daemon has no old grants; persistent kernel pins still protect open files.
    /\ daemon' = 1 /\ connected' = FALSE
    /\ generation' = IF handle = "none" THEN generation + 1 ELSE generation
    /\ phase' = "idle"
    /\ UNCHANGED <<epoch, expired, pending, cached, floor, notified, ack, ackEpoch,
                   handle, readsCurrent, handlesProtected, grantsOutsideMutation>>

Next == Issue \/ DeliverGrant \/ LocalOpen \/ LocalStatOrRead \/ Close
        \/ BeginMutation \/ ReceiveRevoke \/ ReceiveAck \/ Expire \/ Timeout
        \/ Mutate \/ SkipBusy \/ Disconnect \/ Restart
GrantType == [present : BOOLEAN, epoch : 0..MaxEpoch,
              generation : 0..MaxEpoch, daemon : 0..1]
TypeOK ==
    /\ epoch \in 0..MaxEpoch /\ generation \in 0..MaxEpoch /\ daemon \in 0..1
    /\ phase \in {"idle", "revoke", "ready"}
    /\ connected \in BOOLEAN /\ expired \in BOOLEAN
    /\ pending \in GrantType /\ cached \in GrantType /\ floor \in 0..MaxEpoch
    /\ notified \in BOOLEAN /\ ack \in BOOLEAN /\ ackEpoch \in 0..MaxEpoch
    /\ handle \in {"none", "local"}
    /\ readsCurrent \in BOOLEAN /\ handlesProtected \in BOOLEAN
    /\ grantsOutsideMutation \in BOOLEAN
ReadsCurrent == readsCurrent
HandlesProtected == handlesProtected
GrantsOutsideMutation == grantsOutsideMutation
Spec == Init /\ [][Next]_vars
=============================================================================
