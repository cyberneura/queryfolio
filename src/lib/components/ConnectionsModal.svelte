<script lang="ts">
  // Connection editor (CYBERNEURA-DEV-1299): add / edit / duplicate / delete the entries of
  // `servers` in config.yml. Secrets never come here (src-tauri/src/connections.rs fills them in
  // on save); see the "Connection editor" section of AGENTS.md.
  import { onMount } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { toast } from "svelte-sonner";
  import type { SaveOutcome } from "$lib/api";
  import appStore from "$lib/stores/app.svelte";
  import {
    PASSWORD_COMMAND_EXAMPLE,
    PASSWORD_COMMAND_WARNING,
    SSL_MODES,
    allServers,
    buildPatch,
    canonicalEngine,
    duplicateServer,
    effectiveEngine,
    fieldsFor,
    templateEngines,
    itemsFromView,
    newServer,
    schemaLabel,
    secretWillBeSet,
    serverEdit,
    validateItems,
    type ConnectionsEditorView,
    type GroupItem,
    type Item,
    type SecretKey,
    type ServerItem,
  } from "$lib/connectionsForm";

  interface Props {
    onClose: () => void;
    /// Connection to select first (opened from a connection's context menu)
    initialName?: string | null;
    /// Opens Config > View override config yaml (when servers come from the override)
    onViewOverride: () => void;
    onEditAsText: () => void;
  }

  let { onClose, initialName = null, onViewOverride, onEditAsText }: Props = $props();

  let view = $state<ConnectionsEditorView | null>(null);
  let loadError = $state<string | null>(null);
  let items = $state<Item[]>([]);
  let initialPatch = $state("");
  let selectedId = $state<number | null>(null);
  let saving = $state(false);
  let saveError = $state<string | null>(null);
  let confirmComments = $state(false);
  let confirmDiscard = $state(false);
  let testing = $state(false);
  let testResult = $state<{ ok: boolean; text: string } | null>(null);
  let dialog = $state<HTMLDivElement | null>(null);

  async function load() {
    loadError = null;
    try {
      const v = await invoke<ConnectionsEditorView>("get_connections_editor");
      view = v;
      const engines = templateEngines(v.templates);
      items = v.local ? itemsFromView(v.local, engines) : [];
      initialPatch = JSON.stringify(buildPatch(items, v.local?.revision ?? "", engines));
      const first = allServers(items).find((s) => s.value.name === initialName) ?? allServers(items)[0];
      selectedId = first?.id ?? null;
    } catch (e) {
      loadError = String(e);
    }
  }

  onMount(() => {
    dialog?.focus();
    void load();
  });

  const editable = $derived(!!view?.editable);
  const tplEngines = $derived(templateEngines(view?.templates ?? []));
  const patch = $derived(buildPatch(items, view?.local?.revision ?? "", tplEngines));
  const dirty = $derived(JSON.stringify(patch) !== initialPatch);
  const problem = $derived(validateItems(items));
  const groups = $derived(items.filter((i): i is GroupItem => i.kind === "group"));
  const selected = $derived(
    items.find((i) => i.id === selectedId) ??
      allServers(items).find((s) => s.id === selectedId) ??
      null,
  );
  const selectedServer = $derived(selected?.kind === "server" ? selected : null);
  const engine = $derived(selectedServer ? effectiveEngine(selectedServer.value, tplEngines) : "");
  const fields = $derived(selectedServer ? fieldsFor(engine) : []);

  /// The list that holds `id` (the top level or a group's servers)
  function containerOf(id: number): Item[] | ServerItem[] {
    if (items.some((i) => i.id === id)) return items;
    for (const g of groups) if (g.servers.some((s) => s.id === id)) return g.servers;
    return items;
  }

  function groupOf(id: number): GroupItem | null {
    return groups.find((g) => g.servers.some((s) => s.id === id)) ?? null;
  }

  function addServer() {
    const item = newServer();
    const group = selectedId !== null ? groupOf(selectedId) : null;
    if (group) group.servers.push(item);
    else items.push(item);
    selectedId = item.id;
    testResult = null;
  }

  function addGroup() {
    const group: GroupItem = { kind: "group", id: Date.now(), groupName: "New group", color: "", servers: [] };
    items.push(group);
    selectedId = group.id;
  }

  function duplicateSelected() {
    if (!selectedServer) return;
    const names = allServers(items).map((s) => String(s.value.name ?? ""));
    const copy = duplicateServer($state.snapshot(selectedServer) as ServerItem, names);
    const list = containerOf(selectedServer.id) as ServerItem[];
    list.splice(list.findIndex((s) => s.id === selectedServer.id) + 1, 0, copy);
    selectedId = copy.id;
    testResult = null;
  }

  function deleteSelected() {
    if (selectedId === null) return;
    const list = containerOf(selectedId);
    const index = list.findIndex((i) => i.id === selectedId);
    if (index < 0) return;
    list.splice(index, 1);
    selectedId = allServers(items)[0]?.id ?? items[0]?.id ?? null;
    testResult = null;
  }

  function move(delta: number) {
    if (selectedId === null) return;
    const list = containerOf(selectedId);
    const index = list.findIndex((i) => i.id === selectedId);
    const to = index + delta;
    if (index < 0 || to < 0 || to >= list.length) return;
    const [item] = list.splice(index, 1);
    list.splice(to, 0, item as never);
  }

  /// Moves the selected server into a group (or to the top level)
  function setGroup(groupId: number | null) {
    if (!selectedServer) return;
    const server = selectedServer;
    const from = containerOf(server.id) as ServerItem[];
    from.splice(from.findIndex((s) => s.id === server.id), 1);
    const target = groups.find((g) => g.id === groupId);
    if (target) target.servers.push(server);
    else items.push(server);
    selectedId = server.id;
  }

  function setField(key: string, value: unknown) {
    if (!selectedServer) return;
    if (value === "" || value === undefined || value === null) delete selectedServer.value[key];
    else selectedServer.value[key] = value;
    testResult = null;
  }

  /// A checkbox: unchecked writes an explicit false when a template (which could set it to true)
  /// or the file had the key; otherwise the key is left out
  function setFlag(key: string, checked: boolean) {
    if (!selectedServer) return;
    if (checked) setField(key, true);
    else if (selectedServer.value.template || key in selectedServer.value) setField(key, false);
    testResult = null;
  }

  const str = (v: unknown) => (typeof v === "string" ? v : v === undefined || v === null ? "" : String(v));
  const tunnel = $derived((selectedServer?.value.ssh_tunnel ?? null) as Record<string, unknown> | null);

  function setTunnel(key: string, value: unknown) {
    if (!selectedServer) return;
    const t = { ...((selectedServer.value.ssh_tunnel as Record<string, unknown>) ?? {}) };
    if (value === "" || value === undefined) delete t[key];
    else t[key] = value;
    selectedServer.value.ssh_tunnel = t;
    testResult = null;
  }

  function setSecret(key: SecretKey, text: string) {
    if (!selectedServer) return;
    if (text === "") delete selectedServer.secretEdits[key];
    else selectedServer.secretEdits[key] = { op: "set", value: text };
    testResult = null;
  }

  function removeSecret(key: SecretKey, remove: boolean) {
    if (!selectedServer) return;
    if (remove) selectedServer.secretEdits[key] = { op: "unset" };
    else delete selectedServer.secretEdits[key];
  }

  const typed = (key: SecretKey) => {
    const edit = selectedServer?.secretEdits[key];
    return edit?.op === "set" ? edit.value : "";
  };
  const removing = (key: SecretKey) => selectedServer?.secretEdits[key]?.op === "unset";
  const stored = (key: SecretKey) => !!selectedServer && selectedServer.secretsSet.includes(key);

  const usesCommand = $derived(typeof selectedServer?.value.password_command === "string");

  async function testConnection() {
    if (!selectedServer || testing) return;
    testing = true;
    testResult = null;
    try {
      const message = await invoke<string>("test_connection", {
        server: serverEdit($state.snapshot(selectedServer) as ServerItem, tplEngines),
        revision: view?.local?.revision ?? "",
      });
      testResult = { ok: true, text: message };
    } catch (e) {
      testResult = { ok: false, text: String(e) };
    } finally {
      testing = false;
    }
  }

  async function save(allowDropComments: boolean) {
    if (!view?.local || problem || saving) return;
    if (!dirty) {
      onClose();
      return;
    }
    saving = true;
    saveError = null;
    try {
      const outcome = await invoke<SaveOutcome>("save_connections", {
        patch: $state.snapshot(patch),
        allowDropComments,
      });
      if (outcome.status === "needs_comment_confirmation") {
        confirmComments = true;
        return;
      }
      confirmComments = false;
      if (outcome.backup_created) {
        toast.info("Kept a copy of the previous config", { description: outcome.backup_created });
      }
      if (await appStore.reloadConnections()) {
        toast.success("Connections saved");
        onClose();
      } else {
        saveError = `Saved, but reloading failed: ${appStore.errorMessage ?? "unknown error"}`;
      }
    } catch (e) {
      saveError = String(e);
    } finally {
      saving = false;
    }
  }

  function requestClose() {
    if (dirty && !saving) {
      confirmDiscard = true;
      return;
    }
    onClose();
  }

  const input =
    "w-full rounded border border-zinc-700 bg-zinc-950 px-2 py-1 font-mono text-xs text-zinc-200 disabled:cursor-not-allowed disabled:opacity-60";
  const small =
    "shrink-0 whitespace-nowrap rounded border border-zinc-600 px-2 py-0.5 text-[11px] text-zinc-300 hover:bg-zinc-800 disabled:opacity-40";
  const label = "flex flex-col gap-1";
  const muted = "text-zinc-500";
</script>

<svelte:window
  onkeydown={(e) => {
    if (e.key === "Escape") {
      e.preventDefault();
      if (confirmComments) confirmComments = false;
      else if (confirmDiscard) confirmDiscard = false;
      else requestClose();
    }
  }}
/>

{#snippet serverRow(server: ServerItem, indent: boolean)}
  <button
    type="button"
    class={`flex w-full items-center gap-1.5 truncate px-2 py-1 text-left text-xs ${indent ? "pl-5" : ""} ${selectedId === server.id ? "bg-zinc-700 text-zinc-100" : "text-zinc-300 hover:bg-zinc-800"}`}
    data-annotate={`row-connection-${server.id}`}
    onclick={() => {
      selectedId = server.id;
      testResult = null;
    }}
  >
    <i class="bi bi-database text-zinc-500" aria-hidden="true"></i>
    <span class="truncate">{str(server.value.name) || "(unnamed)"}</span>
    <span class="ml-auto text-[10px] text-zinc-500">{str(server.value.engine) || str(server.value.template)}</span>
  </button>
{/snippet}

<div
  class="fixed inset-0 z-10 flex items-center justify-center bg-black/60"
  role="presentation"
  data-annotate="backdrop-connections-modal"
  data-modal
  onclick={(e) => {
    if (e.target === e.currentTarget) requestClose();
  }}
>
  <div
    class="flex h-[85vh] w-[920px] max-w-[95vw] flex-col overflow-hidden rounded-lg border border-zinc-700 bg-zinc-900 shadow-xl outline-none"
    role="dialog"
    aria-modal="true"
    aria-labelledby="connections-title"
    tabindex="-1"
    bind:this={dialog}
  >
    <div class="flex items-center justify-between border-b border-zinc-800 px-4 py-3">
      <div id="connections-title" class="text-sm font-semibold text-zinc-200">Connections</div>
      <button class={small} data-annotate="button-connections-edit-as-text" disabled={dirty} title="Edit config.yml as text (keeps your comments)" onclick={onEditAsText}>
        <i class="bi bi-file-earmark-code"></i> Edit as text
      </button>
    </div>

    {#if loadError}
      <p class="whitespace-pre-wrap p-4 font-mono text-xs text-red-400">{loadError}</p>
    {:else if !view}
      <p class="p-4 text-xs text-zinc-500">Loading...</p>
    {:else}
      {#if view.readonly_reason}
        <div class="flex items-center gap-2 border-b border-zinc-800 px-4 py-2 text-xs text-amber-400" data-annotate="text-connections-readonly">
          <span class="flex-1">Read-only: {view.readonly_reason}</span>
          {#if view.servers_overridden}
            <button class={small} data-annotate="button-view-override-yaml" title="Show the YAML config_override_command returns, to edit and copy it back to 1Password" onclick={onViewOverride}>
              View override YAML
            </button>
          {/if}
        </div>
      {/if}
      {#if editable && view.has_comments}
        <p class="border-b border-zinc-800 px-4 py-2 text-xs text-zinc-500">
          config.yml has comments. Saving here rewrites the file without them (you will be asked first). Use "Edit as text" to keep them.
        </p>
      {/if}

      <fieldset class="flex min-h-0 flex-1" disabled={saving || !editable}>
        <!-- List -->
        <div class="flex w-64 shrink-0 flex-col border-r border-zinc-800">
          <div class="min-h-0 flex-1 overflow-y-auto py-1" data-annotate="list-connections">
            {#each items as item (item.id)}
              {#if item.kind === "group"}
                <button
                  type="button"
                  class={`flex w-full items-center gap-1.5 px-2 py-1 text-left text-xs font-semibold ${selectedId === item.id ? "bg-zinc-700 text-zinc-100" : "text-zinc-400 hover:bg-zinc-800"}`}
                  data-annotate={`row-group-${item.id}`}
                  onclick={() => (selectedId = item.id)}
                >
                  <i class="bi bi-folder2" aria-hidden="true"></i>
                  {item.groupName || "(unnamed group)"}
                </button>
                {#each item.servers as server (server.id)}
                  {@render serverRow(server, true)}
                {/each}
              {:else}
                {@render serverRow(item, false)}
              {/if}
            {/each}
            {#if items.length === 0}
              <p class="px-3 py-2 text-xs text-zinc-500">No connections yet. Add one below.</p>
            {/if}
          </div>
          <div class="flex flex-wrap gap-1 border-t border-zinc-800 p-2">
            <button class={small} data-annotate="button-add-connection" onclick={addServer}><i class="bi bi-plus"></i> Connection</button>
            <button class={small} data-annotate="button-add-group" onclick={addGroup}><i class="bi bi-folder-plus"></i> Group</button>
            <button class={small} data-annotate="button-duplicate-connection" disabled={!selectedServer} onclick={duplicateSelected}>Duplicate</button>
            <button class={small} data-annotate="button-delete-connection" disabled={selectedId === null} onclick={deleteSelected}>Delete</button>
            <button class={small} data-annotate="button-move-up" title="Move up" disabled={selectedId === null} onclick={() => move(-1)}><i class="bi bi-arrow-up"></i></button>
            <button class={small} data-annotate="button-move-down" title="Move down" disabled={selectedId === null} onclick={() => move(1)}><i class="bi bi-arrow-down"></i></button>
          </div>
        </div>

        <!-- Detail -->
        <div class="min-w-0 flex-1 overflow-y-auto p-4 text-xs text-zinc-300">
          {#if selected?.kind === "group"}
            <div class="flex flex-col gap-3">
              <label class={label}>
                <span>Group name</span>
                <input class={input} data-annotate="input-group-name" value={selected.groupName} oninput={(e) => (selected.groupName = e.currentTarget.value)} />
              </label>
              <label class={label}>
                <span>Color <span class={muted}>(CSS color name or #rrggbb; shown on the group's connections)</span></span>
                <input class={`${input} max-w-48`} placeholder="e.g. tomato" data-annotate="input-group-color" value={selected.color} oninput={(e) => (selected.color = e.currentTarget.value)} />
              </label>
              <p class={muted}>Deleting a group deletes its connections too. Move them out first (select a connection, then change its group).</p>
            </div>
          {:else if selectedServer}
            {@const server = selectedServer}
            <div class="flex flex-col gap-3">
              <div class="grid grid-cols-2 gap-3">
                <label class={label}>
                  <span>Name</span>
                  <input class={input} data-annotate="input-connection-name" value={str(server.value.name)} oninput={(e) => setField("name", e.currentTarget.value)} />
                </label>
                <label class={label}>
                  <span>Engine</span>
                  <select class={input} data-annotate="select-connection-engine" value={canonicalEngine(str(server.value.engine))} onchange={(e) => setField("engine", e.currentTarget.value)}>
                    {#if !server.value.engine}<option value="">(from the template)</option>{/if}
                    {#each view.engines as engine (engine)}
                      <option value={engine}>{engine}</option>
                    {/each}
                  </select>
                </label>
                <label class={`${label} col-span-2`}>
                  <span>Description <span class={muted}>(optional)</span></span>
                  <input class={input} data-annotate="input-connection-description" value={str(server.value.description)} oninput={(e) => setField("description", e.currentTarget.value)} />
                </label>
                <label class={label}>
                  <span>Template <span class={muted}>(server_templates)</span></span>
                  <select class={input} data-annotate="select-connection-template" value={str(server.value.template)} onchange={(e) => setField("template", e.currentTarget.value)}>
                    <option value="">(none)</option>
                    {#each [...new Set([...view.templates.map((t) => t.name), str(server.value.template)].filter((t) => t))] as t (t)}
                      <option value={t}>{t}</option>
                    {/each}
                  </select>
                </label>
                <label class={label}>
                  <span>Group</span>
                  <select class={input} data-annotate="select-connection-group" value={String(groupOf(server.id)?.id ?? "")} onchange={(e) => setGroup(e.currentTarget.value ? Number(e.currentTarget.value) : null)}>
                    <option value="">(none)</option>
                    {#each groups as g (g.id)}
                      <option value={String(g.id)}>{g.groupName || "(unnamed group)"}</option>
                    {/each}
                  </select>
                </label>
              </div>

              <div class="grid grid-cols-2 gap-3">
                {#if fields.includes("host")}
                  <label class={label}>
                    <span>{engine === "dynamodb" ? "Endpoint host (dynamodb-local only)" : "Host"}</span>
                    <input class={input} placeholder="localhost" data-annotate="input-connection-host" value={str(server.value.host)} oninput={(e) => setField("host", e.currentTarget.value)} />
                  </label>
                {/if}
                {#if fields.includes("port")}
                  <label class={label}>
                    <span>Port <span class={muted}>(empty = default)</span></span>
                    <input class={`${input} max-w-32`} inputmode="numeric" data-annotate="input-connection-port" value={str(server.value.port)} oninput={(e) => setField("port", e.currentTarget.value.trim() === "" ? undefined : Number(e.currentTarget.value))} />
                  </label>
                {/if}
                {#if fields.includes("schema")}
                  <label class={label}>
                    <span>{schemaLabel(engine)}</span>
                    <input class={input} data-annotate="input-connection-schema" value={str(server.value.schema)} oninput={(e) => setField("schema", e.currentTarget.value)} />
                  </label>
                {/if}
                {#if fields.includes("user")}
                  <label class={label}>
                    <span>{engine === "dynamodb" ? "Access key ID" : "User"}</span>
                    <input class={input} autocomplete="off" data-annotate="input-connection-user" value={str(server.value.user)} oninput={(e) => setField("user", e.currentTarget.value)} />
                  </label>
                {/if}
                {#if fields.includes("aws_profile")}
                  <label class={label}>
                    <span>AWS profile <span class={muted}>(~/.aws)</span></span>
                    <input class={input} data-annotate="input-connection-aws-profile" value={str(server.value.aws_profile)} oninput={(e) => setField("aws_profile", e.currentTarget.value)} />
                  </label>
                {/if}
                {#if fields.includes("ssl_mode")}
                  <label class={label}>
                    <span>TLS (ssl_mode)</span>
                    <select class={input} data-annotate="select-connection-ssl-mode" value={str(server.value.ssl_mode)} onchange={(e) => setField("ssl_mode", e.currentTarget.value)}>
                      {#each SSL_MODES as mode (mode)}
                        <option value={mode}>{mode === "" ? "Default (prefer)" : mode}</option>
                      {/each}
                    </select>
                  </label>
                {/if}
                {#if fields.includes("ssl_root_cert")}
                  <label class={label}>
                    <span>Root CA file <span class={muted}>(verify-ca / verify-full)</span></span>
                    <input class={input} data-annotate="input-connection-ssl-root-cert" value={str(server.value.ssl_root_cert)} oninput={(e) => setField("ssl_root_cert", e.currentTarget.value)} />
                  </label>
                {/if}
                {#if fields.includes("tls")}
                  <label class="flex items-center gap-2 pt-4">
                    <input type="checkbox" data-annotate="checkbox-connection-tls" checked={server.value.tls === true} onchange={(e) => setFlag("tls", e.currentTarget.checked)} />
                    Use https (tls)
                  </label>
                {/if}
              </div>

              {#if fields.includes("password")}
                <div class="flex flex-col gap-1.5 rounded border border-zinc-800 p-2">
                  <span>{engine === "dynamodb" ? "Secret access key" : "Password"}</span>
                  <label class="flex items-center gap-2">
                    <input type="radio" name="password-mode" data-annotate="radio-password-command" checked={usesCommand} onchange={() => setField("password_command", str(server.value.password_command) || PASSWORD_COMMAND_EXAMPLE)} />
                    Get it with a command — password_command (1Password recommended)
                  </label>
                  {#if usesCommand}
                    <div class="ml-5 flex flex-col gap-1">
                      <input class={input} placeholder={PASSWORD_COMMAND_EXAMPLE} data-annotate="input-password-command" value={str(server.value.password_command)} oninput={(e) => (server.value.password_command = e.currentTarget.value)} />
                      <p class="text-[11px] text-amber-400">{PASSWORD_COMMAND_WARNING}</p>
                      {#if stored("password")}
                        <p class={muted}>The password stored in config.yml will be removed when you save.</p>
                      {/if}
                    </div>
                  {/if}
                  <label class="flex items-center gap-2">
                    <input type="radio" name="password-mode" data-annotate="radio-password-direct" checked={!usesCommand} onchange={() => {
                      // With a template (which could supply a command), cancel it explicitly
                      if (server.value.template) server.value.password_command = null;
                      else setField("password_command", undefined);
                    }} />
                    Enter it here (stored as plain text in config.yml)
                  </label>
                  {#if !usesCommand}
                    <div class="ml-5 flex items-center gap-2">
                      <input
                        class={input}
                        type="password"
                        autocomplete="off"
                        placeholder={stored("password") ? "A password is stored. Type a new one to replace it" : "(no password)"}
                        data-annotate="input-connection-password"
                        disabled={removing("password")}
                        value={typed("password")}
                        oninput={(e) => setSecret("password", e.currentTarget.value)}
                      />
                      {#if stored("password")}
                        <label class="flex shrink-0 items-center gap-1 text-zinc-400">
                          <input type="checkbox" data-annotate="checkbox-remove-password" checked={removing("password")} onchange={(e) => removeSecret("password", e.currentTarget.checked)} />
                          Remove
                        </label>
                      {/if}
                    </div>
                  {/if}
                </div>
              {/if}

              {#if fields.includes("ssh_tunnel")}
                <div class="flex flex-col gap-1.5 rounded border border-zinc-800 p-2">
                  <label class="flex items-center gap-2">
                    <input
                      type="checkbox"
                      data-annotate="checkbox-ssh-tunnel"
                      checked={tunnel !== null}
                      onchange={(e) => {
                        if (e.currentTarget.checked) server.value.ssh_tunnel = {};
                        else delete server.value.ssh_tunnel;
                        testResult = null;
                      }}
                    />
                    Connect through an SSH tunnel
                  </label>
                  {#if tunnel}
                    <div class="ml-5 flex flex-col gap-1.5">
                      <label class="flex items-center gap-2">
                        <input type="radio" name="tunnel-mode" data-annotate="radio-tunnel-ssh-config" checked={typeof tunnel.ssh_config === "string"} onchange={() => setTunnel("ssh_config", str(tunnel.ssh_config) || " ")} />
                        Use a Host from ~/.ssh/config (ProxyJump etc. work; OpenSSH handles authentication)
                      </label>
                      <label class="flex items-center gap-2">
                        <input type="radio" name="tunnel-mode" data-annotate="radio-tunnel-host" checked={typeof tunnel.ssh_config !== "string"} onchange={() => setTunnel("ssh_config", undefined)} />
                        Enter the host and user
                      </label>
                      {#if typeof tunnel.ssh_config === "string"}
                        <input class={input} placeholder="Host alias in ~/.ssh/config" data-annotate="input-tunnel-ssh-config" value={str(tunnel.ssh_config).trim()} oninput={(e) => setTunnel("ssh_config", e.currentTarget.value || " ")} />
                      {:else}
                        <div class="grid grid-cols-2 gap-1.5">
                          <input class={input} placeholder="host" data-annotate="input-tunnel-host" value={str(tunnel.host)} oninput={(e) => setTunnel("host", e.currentTarget.value)} />
                          <input class={input} data-annotate="input-tunnel-port" placeholder="port (22)" inputmode="numeric" value={str(tunnel.port)} oninput={(e) => setTunnel("port", e.currentTarget.value.trim() === "" ? undefined : Number(e.currentTarget.value))} />
                          <input class={input} placeholder="user" data-annotate="input-tunnel-user" value={str(tunnel.user)} oninput={(e) => setTunnel("user", e.currentTarget.value)} />
                          <input class={input} data-annotate="input-tunnel-private-key-path" placeholder="private key path (optional)" value={str(tunnel.private_key_path)} oninput={(e) => setTunnel("private_key_path", e.currentTarget.value)} />
                          <input class={`${input} col-span-2`} data-annotate="input-tunnel-identity-agent" placeholder="ssh-agent socket (optional; e.g. the 1Password agent)" value={str(tunnel.identity_agent)} oninput={(e) => setTunnel("identity_agent", e.currentTarget.value)} />
                          {#each [["ssh_tunnel.password", "SSH password"], ["ssh_tunnel.private_key_passphrase", "Key passphrase"]] as [key, text] (key)}
                            <div class="col-span-2 flex items-center gap-2">
                              <input
                                class={input}
                                type="password"
                                autocomplete="off"
                                data-annotate={`input-${key}`}
                                placeholder={stored(key as SecretKey) ? `${text}: stored (type to replace)` : `${text} (optional)`}
                                disabled={removing(key as SecretKey)}
                                value={typed(key as SecretKey)}
                                oninput={(e) => setSecret(key as SecretKey, e.currentTarget.value)}
                              />
                              {#if stored(key as SecretKey)}
                                <label class="flex shrink-0 items-center gap-1 text-zinc-400">
                                  <input type="checkbox" data-annotate={`checkbox-remove-${key}`} checked={removing(key as SecretKey)} onchange={(e) => removeSecret(key as SecretKey, e.currentTarget.checked)} />
                                  Remove
                                </label>
                              {/if}
                            </div>
                          {/each}
                        </div>
                      {/if}
                    </div>
                  {/if}
                </div>
              {/if}

              <div class="grid grid-cols-2 gap-3">
                <label class="flex items-center gap-2" title="Reject INSERT / UPDATE / DELETE / DDL on this connection">
                  <input type="checkbox" data-annotate="checkbox-readonly" checked={server.value.readonly === true} onchange={(e) => setFlag("readonly", e.currentTarget.checked)} />
                  Read-only connection
                </label>
                <label class="flex items-center gap-2" title="Allow UPDATE / DELETE without WHERE, DROP and TRUNCATE (with a confirmation)">
                  <input type="checkbox" data-annotate="checkbox-allow-dangerous" checked={server.value.allow_dangerous_statements === true} onchange={(e) => setFlag("allow_dangerous_statements", e.currentTarget.checked)} />
                  Allow dangerous statements
                </label>
                <label class={label}>
                  <span>Color <span class={muted}>(CSS color name or #rrggbb)</span></span>
                  <input class={input} placeholder="e.g. crimson for production" data-annotate="input-connection-color" value={str(server.value.color)} oninput={(e) => setField("color", e.currentTarget.value)} />
                </label>
                <label class={label}>
                  <span>Query file folder <span class={muted}>(folder_name; empty = derived)</span></span>
                  <input class={input} data-annotate="input-connection-folder-name" value={str(server.value.folder_name)} oninput={(e) => setField("folder_name", e.currentTarget.value)} />
                </label>
              </div>
              {#if server.origin !== null && server.value.folder_name === undefined}
                <p class={muted}>Changing host / engine / database / user changes the derived query file folder; existing files stay in the old folder. Set folder_name to keep it.</p>
              {/if}

              {#if testResult}
                <p class={`whitespace-pre-wrap text-[11px] ${testResult.ok ? "text-emerald-400" : "text-red-400"}`} data-annotate="text-test-connection-result">{testResult.text}</p>
              {/if}
            </div>
          {:else}
            <p class={muted}>Select a connection, or add one.</p>
          {/if}
        </div>
      </fieldset>
    {/if}

    <div class="flex flex-col gap-2 border-t border-zinc-800 px-4 py-3 text-xs">
      {#if confirmComments && view}
        <div class="flex flex-col gap-2 rounded border border-amber-700 bg-amber-950/40 p-2">
          <p class="text-amber-200">
            config.yml has comments. Saving from this window rewrites the file and removes them.
            {view.backup_exists ? "The copy made before the first save is at" : "A copy of the current file is kept at"}
            <span class="font-mono">{view.backup_path}</span>.
          </p>
          <div class="flex justify-end gap-2">
            <button class="rounded border border-zinc-600 px-3 py-1 text-zinc-300" data-annotate="button-connections-comments-cancel" onclick={() => (confirmComments = false)}>Cancel</button>
            <button class="rounded bg-amber-600 px-3 py-1 text-white hover:bg-amber-500" data-annotate="button-connections-comments-save" disabled={saving} onclick={() => save(true)}>Save and remove comments</button>
          </div>
        </div>
      {:else if confirmDiscard}
        <div class="flex items-center justify-end gap-2">
          <span class="mr-auto text-amber-300">Discard your changes?</span>
          <button class="rounded border border-zinc-600 px-3 py-1 text-zinc-300" data-annotate="button-connections-keep-editing" onclick={() => (confirmDiscard = false)}>Keep editing</button>
          <button class="rounded bg-red-700 px-3 py-1 text-white hover:bg-red-600" data-annotate="button-connections-discard" onclick={onClose}>Discard</button>
        </div>
      {:else}
        <div class="flex items-center gap-2">
          <button
            class="rounded border border-zinc-600 px-3 py-1 text-zinc-300 hover:bg-zinc-800 disabled:opacity-40"
            data-annotate="button-test-connection"
            title="Connect with these settings without saving them"
            disabled={!selectedServer || testing || !view?.local}
            onclick={testConnection}
          >
            {testing ? "Testing..." : "Test connection"}
          </button>
          {#if problem && dirty}
            <span class="text-amber-400" data-annotate="text-connections-problem">{problem}</span>
          {/if}
          {#if saveError}
            <span class="whitespace-pre-wrap text-red-400" data-annotate="text-connections-save-error">{saveError}</span>
          {/if}
          <span class="flex-1"></span>
          <button class="rounded border border-zinc-600 px-3 py-1 text-zinc-300 hover:bg-zinc-800" data-annotate="button-connections-close" onclick={requestClose}>{dirty ? "Cancel" : "Close"}</button>
          <button
            class="rounded bg-blue-600 px-3 py-1 text-white hover:bg-blue-500 disabled:opacity-40"
            data-annotate="button-connections-save"
            disabled={!editable || !dirty || saving || !!problem}
            onclick={() => save(false)}
          >
            {saving ? "Saving..." : "Save"}
          </button>
        </div>
      {/if}
    </div>
  </div>
</div>
