<svelte:options runes={true} />

<script lang="ts">
  import { onMount, tick } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import { invoke } from "@tauri-apps/api/core";
  import Pill from "$lib/Pill.svelte";

  type State = "idle" | "recording" | "transcribing" | "error";
  type PillSnapshot = {
    state: State;
    errorText: string;
    seq: number;
  };
  type PillStateEvent = {
    state: State;
    seq: number;
  };
  type PillErrorEvent = {
    message: string;
    seq: number;
  };

  const BAR_COUNT = 20;
  let pillState: State = $state("idle");
  let errorText: string = $state("");
  let spectrum: number[] = $state(Array(BAR_COUNT).fill(0));

  onMount(() => {
    let hideTimer: ReturnType<typeof setTimeout> | undefined;
    let errorTimer: ReturnType<typeof setTimeout> | undefined;
    let transitionSeq = 0;
    let latestBackendSeq = 0;

    const clearTimer = (id: ReturnType<typeof setTimeout> | undefined) => {
      if (id) clearTimeout(id);
    };

    const clearPendingTransitions = () => {
      clearTimer(hideTimer);
      clearTimer(errorTimer);
      hideTimer = undefined;
      errorTimer = undefined;
      transitionSeq += 1;
    };

    const repaintAfterDomFlush = async () => {
      await tick();
      await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
      invoke("repaint_window").catch(console.error);
    };

    const scheduleHide = () => {
      clearTimer(hideTimer);
      const seq = transitionSeq;
      hideTimer = setTimeout(() => {
        if (seq !== transitionSeq) return;
        // Logical hide only: backend keeps the transparent click-through
        // window alive so WebView2 never sits in hidden/background state.
        invoke("hide_window").catch(console.error);
        hideTimer = undefined;
      }, 520);
    };

    const applySnapshot = (snapshot: PillSnapshot) => {
      if (snapshot.seq < latestBackendSeq) return;
      latestBackendSeq = snapshot.seq;

      if (snapshot.state === "idle") {
        pillState = "idle";
        return;
      }

      clearPendingTransitions();
      if (snapshot.state === "recording") {
        spectrum = Array(BAR_COUNT).fill(0);
      }
      if (snapshot.state === "error") {
        errorText = snapshot.errorText;
      }
      pillState = snapshot.state;
      repaintAfterDomFlush();

      if (snapshot.state === "error") {
        const seq = transitionSeq;
        errorTimer = setTimeout(() => {
          if (seq !== transitionSeq) return;
          pillState = "idle";
          scheduleHide();
          errorTimer = undefined;
        }, 3500);
      }
    };

    let mounted = true;
    const unlisteners: Array<Promise<() => void>> = [
      listen<PillStateEvent>("state-changed", (e) => {
        if (e.payload.seq < latestBackendSeq) return;
        latestBackendSeq = e.payload.seq;
        const next = e.payload.state;
        clearPendingTransitions();
        if (next === "recording") {
          spectrum = Array(BAR_COUNT).fill(0);
          pillState = next;
          repaintAfterDomFlush();
          return;
        }
        if (next === "idle") {
          // Drive morph-out via {#if}; notify backend once transition ends.
          pillState = "idle";
          // morphPill duration is 460ms; pad slightly so we don't cut it off.
          scheduleHide();
          return;
        }
        pillState = next;
        repaintAfterDomFlush();
      }),
      listen<number[]>("audio-spectrum", (e) => {
        spectrum = e.payload;
      }),
      listen<PillErrorEvent>("transcribe-error", (e) => {
        if (e.payload.seq < latestBackendSeq) return;
        latestBackendSeq = e.payload.seq;
        clearPendingTransitions();
        errorText = e.payload.message;
        pillState = "error";
        repaintAfterDomFlush();
        // Hold the error on screen long enough to read, then morph out.
        const seq = transitionSeq;
        errorTimer = setTimeout(() => {
          if (seq !== transitionSeq) return;
          pillState = "idle";
          scheduleHide();
          errorTimer = undefined;
        }, 3500);
      }),
    ];
    Promise.all(unlisteners)
      .then(() => {
        if (!mounted) return;
        invoke<PillSnapshot>("pill_frontend_ready")
          .then((snapshot) => {
            if (mounted) applySnapshot(snapshot);
          })
          .catch(console.error);
      })
      .catch(console.error);
    return () => {
      mounted = false;
      clearPendingTransitions();
      unlisteners.forEach((p) => p.then((u) => u()));
    };
  });
</script>

<div class="stage">
  <Pill status={pillState} {spectrum} {errorText} />
</div>

<style>
  :global(html, body) {
    background: transparent !important;
    margin: 0;
    overflow: hidden;
    font-family:
      -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif;
  }
  .stage {
    width: 100vw;
    height: 100vh;
    display: flex;
    align-items: center;
    justify-content: center;
  }
</style>
