---------------------------- MODULE Protection ----------------------------
EXTENDS Naturals, Integers, FiniteSets, TLC

\* A single file, bounded generations, two local targets and one archive.
\* Each Next step is a lock/transaction boundary, NOT a Rust instruction.
\* Data is abstracted by its generation; a verified candidate has been read
\* back and synced. Filesystem/WAL durability, media failure, namespace, IPC
\* memory safety and liveness are outside this model.
\*
\* Implementation correspondence:
\* BeginWrite: core/file.rs::open_id invalidates the protected generation.
\* Verify: core/reconcile.rs::prepare_protected checks again after hashing.
\* Publish: reconcile_one / archive_one recheck version under the file lock.
\* Evict: core/reconcile.rs / archive.rs verify retained copies before GC.
\* Read: core/file.rs::ensure_body accepts only current-generation copies.
\* Guards are configurable only to check that the model detects regressions.
CONSTANTS Targets, Local, MaxGeneration,
          CheckGeneration, CheckVerification, CheckRetention, CheckRead
ASSUME /\ Local \subseteq Targets /\ Cardinality(Local) = 2
       /\ Cardinality(Targets) = 3 /\ MaxGeneration > 0

VARIABLES generation, writer, copies, data, checked,
          phase, source, destination, captured, candidate,
          readOK, publishOK
vars == <<generation, writer, copies, data, checked,
          phase, source, destination, captured, candidate, readOK, publishOK>>
Absent == -1
Idle == "idle"
Copying == "copying"
Verified == "verified"
Current == {t \in Targets : copies[t] = generation}
Protected == {t \in Current : checked[t] = generation /\ data[t] = generation}

Init ==
    /\ generation = 0 /\ writer = FALSE
    /\ \E t \in Local :
        /\ copies = [d \in Targets |-> IF d = t THEN 0 ELSE Absent]
        /\ data = copies /\ checked = copies
    /\ phase = Idle
    /\ source = CHOOSE t \in Targets : TRUE
    /\ destination = source /\ captured = 0 /\ candidate = Absent
    /\ readOK = TRUE /\ publishOK = TRUE

BeginWrite(t) ==
    /\ ~writer /\ generation < MaxGeneration
    /\ generation' = generation + 1 /\ writer' = TRUE
    \* A successful single-target mutation. Other replicas remain old.
    /\ copies' = [copies EXCEPT ![t] = generation + 1]
    /\ data' = [data EXCEPT ![t] = generation + 1]
    /\ checked' = [checked EXCEPT ![t] = Absent]
    /\ UNCHANGED <<phase, source, destination, captured, candidate, readOK, publishOK>>

Close == /\ writer /\ writer' = FALSE
         /\ UNCHANGED <<generation, copies, data, checked, phase, source,
                        destination, captured, candidate, readOK, publishOK>>

Verify(t) ==
    /\ ~writer /\ t \in Current /\ data[t] = generation
    /\ checked' = [checked EXCEPT ![t] = generation]
    /\ UNCHANGED <<generation, writer, copies, data, phase, source,
                   destination, captured, candidate, readOK, publishOK>>

StartCopy(s, d) ==
    /\ ~writer /\ phase = Idle /\ s \in Protected /\ d # s
    /\ phase' = Copying /\ source' = s /\ destination' = d
    /\ captured' = generation /\ candidate' = Absent
    /\ UNCHANGED <<generation, writer, copies, data, checked, readOK, publishOK>>

VerifyCandidate ==
    /\ phase = Copying /\ data[source] = captured
    /\ phase' = Verified /\ candidate' = captured
    /\ UNCHANGED <<generation, writer, copies, data, checked, source,
                   destination, captured, readOK, publishOK>>

Publish ==
    /\ phase # Idle /\ ~writer
    /\ (~CheckGeneration \/ captured = generation)
    /\ (~CheckVerification \/ phase = Verified)
    /\ copies' = [copies EXCEPT ![destination] = captured]
    /\ data' = [data EXCEPT ![destination] = candidate]
    /\ checked' = [checked EXCEPT ![destination] = captured]
    /\ publishOK' = (captured = generation)
    /\ phase' = Idle
    /\ UNCHANGED <<generation, writer, source, destination, captured, candidate, readOK>>

AbortCopy ==
    /\ phase # Idle /\ phase' = Idle
    /\ UNCHANGED <<generation, writer, copies, data, checked, source,
                   destination, captured, candidate, readOK, publishOK>>

Evict(t) ==
    /\ ~writer /\ copies[t] # Absent
    /\ (~CheckRetention \/ Protected \ {t} # {})
    /\ copies' = [copies EXCEPT ![t] = Absent]
    /\ data' = [data EXCEPT ![t] = Absent]
    /\ checked' = [checked EXCEPT ![t] = Absent]
    /\ UNCHANGED <<generation, writer, phase, source, destination,
                   captured, candidate, readOK, publishOK>>

Read(t) ==
    /\ copies[t] # Absent /\ (~CheckRead \/ t \in Current)
    /\ readOK' = (copies[t] = generation)
    /\ UNCHANGED <<generation, writer, copies, data, checked, phase,
                   source, destination, captured, candidate, publishOK>>

Next == \/ \E t \in Local : BeginWrite(t)
        \/ Close
        \/ \E t \in Targets : Verify(t) \/ Evict(t) \/ Read(t)
        \/ \E s, d \in Targets : StartCopy(s, d)
        \/ VerifyCandidate \/ Publish \/ AbortCopy

TypeOK ==
    /\ generation \in 0..MaxGeneration /\ writer \in BOOLEAN
    /\ copies \in [Targets -> Absent..MaxGeneration]
    /\ data \in [Targets -> Absent..MaxGeneration]
    /\ checked \in [Targets -> Absent..MaxGeneration]
    /\ phase \in {Idle, Copying, Verified}
    /\ source \in Targets /\ destination \in Targets
    /\ captured \in 0..MaxGeneration /\ candidate \in Absent..MaxGeneration
    /\ readOK \in BOOLEAN /\ publishOK \in BOOLEAN

DataAvailable == \E t \in Current : data[t] = generation
VerifiedCopiesHaveData ==
    \A t \in Targets : checked[t] # Absent =>
        (data[t] = checked[t] /\ copies[t] = checked[t])
ReadsCurrent == readOK
PublishesCurrent == publishOK
Spec == Init /\ [][Next]_vars
=============================================================================
