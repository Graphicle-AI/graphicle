"use strict";

const oauthServices = [["github", "GitHub"], ["slack", "Slack"], ["gdrive", "Google Drive"], ["notion", "Notion"], ["atlassian", "Atlassian (Jira / Confluence)"]];
const sourceServices = [...oauthServices.filter(([id]) => id !== "atlassian"), ["jira", "Jira"], ["confluence", "Confluence"]];
// Who may read a new connection's copies, and the notes under the choice.
// An installed edition can add choices and notes (/extensions.js).
const connectionSharing = [["owner", "Only me"], ["explicit", "All workspace readers — I authorize this copy"]];
const connectionNotes = ["Drive folders include subfolders."];
const workspaceCommand = (operation, args = {}) => api(`${base()}/repositories/command`, {operation, arguments: args});
function can(permission) {
  return state.role === "owner" || ({reviewer: ["collect", "propose", "approve"], collector: ["collect", "propose"], runner: ["propose", "run"], agent: ["propose", "run"], reader: []}[state.role] || []).includes(permission);
}
function applyAccess() {
  for (const [selector, permission] of [["#add-connection", "admin"], ["#configure-model", "admin"], ["#connect-account", "admin"], ["#manage-access", "admin"], ["#new-source", "admin"], ["#upload-source", "admin"], ["#new-note", "approve"], ["#extract", "propose"], ["#add-project", "collect"]]) $(selector).hidden = !can(permission);
  $('#ask-form button[value="answer"]').disabled = !can("run");
  const team = $("#team-settings"); if (team) team.hidden = !can("admin");
  $("#index-maintenance-panel").hidden = !can("admin");
}
// Views and actions an edition does not include stay hidden: data-requires
// names the build.info capability they need (for example change_impact).
function applyEdition() {
  document.querySelectorAll("[data-requires]").forEach(node => { node.hidden = state.build?.[node.dataset.requires] !== true; });
}
async function reloadModels() { state.models = await api(`${base()}/models`); renderModels(); }
async function credentials() { return await command("credential.list"); }

async function configureModel() {
  const secrets = await credentials();
  formDialog("Choose an answering model", "Save model", form => {
    field(form, "Profile name", "name", {value: "answering"});
    const protocol = field(form, "Connection", "protocol", {choices: [["ollama", "Ollama on this device"], ["chat", "OpenAI-compatible chat API"], ["responses", "OpenAI Responses API"], ["anthropic", "Anthropic"], ["gemini", "Google Gemini"], ["azure_chat", "Azure chat API"], ["azure_responses", "Azure Responses API"]]});
    const endpoint = field(form, "API address", "base_url", {value: "http://127.0.0.1:11434"});
    field(form, "Model name", "model", {placeholder: "The model installed or deployed at this address"});
    const defaults = {ollama: "http://127.0.0.1:11434", chat: "https://api.openai.com/v1", responses: "https://api.openai.com/v1", anthropic: "https://api.anthropic.com/v1", gemini: "https://generativelanguage.googleapis.com/v1beta"};
    protocol.addEventListener("change", () => { endpoint.value = defaults[protocol.value] || ""; });
    field(form, "Saved API credential", "credential_ref", {required: false, choices: [["", "No saved credential"], ...secrets.map(s => [`secret:${s.id}`, s.name])]});
    const key = field(form, "Or enter an API key", "api_key", {required: false}); key.type = "password"; key.autocomplete = "off";
    field(form, "Input limit per call", "input_limit", {value: "16000"}).type = "number";
    field(form, "Output limit per call", "output_limit", {value: "2048"}).type = "number";
    field(form, "Structured responses", "structured_output", {choices: [["true", "Server supports JSON schema"], ["false", "Prompt only — for servers without schema support"]]});
    field(form, "Use for", "purpose", {choices: [["answer", "Default answering model"], ["embedding", "Embedding model"], ["optional", "Optional model"]]});
    form.append(el("p", "API keys are encrypted in this workspace. Saving a model does not send an inference request.", "muted"));
  }, async values => {
    let reference = values.credential_ref || null;
    if (values.api_key) reference = (await command("credential.store", {name: `Model: ${values.name}`, value: values.api_key})).credential_ref;
    await command("model.configure", {name: values.name, default: values.purpose === "answer", embedding: values.purpose === "embedding", profile: {protocol: values.protocol, base_url: values.base_url, model: values.model, credential_ref: reference, input_limit: Number(values.input_limit), output_limit: Number(values.output_limit), temperature: null, structured_output: values.structured_output === "true"}});
    await reloadModels(); notice("Model saved. It is ready for your next workflow.");
  });
}

async function openConnectionDialog() {
  const [secrets, accounts] = await Promise.all([credentials(), command("oauth.status")]);
  let sites = [];
  formDialog("Connect a source", "Add connection", form => {
    field(form, "Name", "name");
    const service = field(form, "Service", "kind", {choices: sourceServices});
    const scope = field(form, "Repository", "scope", {placeholder: "owner/repository"});
    const driveMode = field(form, "Drive selection", "drive_mode", {choices: [["folder", "A folder and its subfolders"], ["files", "Individual files"]]});
    const credential = field(form, "Connected account or saved token", "credential_ref", {required: false, choices: [["", "Choose an account, or enter a token below"]]});
    const site = field(form, "Authorized Atlassian site", "site_id", {required: false, choices: [["", "Choose a connected Atlassian account first"]]});
    const siteURL = field(form, "Atlassian site URL", "site_url", {required: false, placeholder: "https://your-team.atlassian.net"});
    const email = field(form, "Atlassian account email (for a new API token)", "username", {required: false});
    const token = field(form, "Or enter a source access token", "token", {required: false}); token.type = "password"; token.autocomplete = "off";
    field(form, "Update frequency", "interval", {choices: [["900", "Every 15 minutes"], ["3600", "Every hour"], ["0", "Sync manually"]]});
    const sharing = field(form, "Who can read these sources?", "sharing", {choices: connectionSharing});
    const labels = () => {
      const atlassian = ["jira", "confluence"].includes(service.value), oauth = credential.value.startsWith("oauth:");
      driveMode.parentElement.hidden = service.value !== "gdrive";
      scope.previousSibling.textContent = ({github: "Repository", slack: "Channel IDs", notion: "Page IDs", gdrive: driveMode.value === "folder" ? "Folder ID" : "File IDs", jira: "Issue selection (JQL)", confluence: "Space key"})[service.value];
      scope.placeholder = ({github: "owner/repository", slack: "C0123456789, C9876543210", notion: "Page IDs, separated by commas", gdrive: driveMode.value === "folder" ? "The ID after /folders/ in the Drive URL" : "File IDs, separated by commas", jira: 'project = "PRODUCT" ORDER BY updated ASC', confluence: "PRODUCT"})[service.value];
      site.parentElement.hidden = !atlassian || !oauth; site.required = atlassian && oauth;
      siteURL.parentElement.hidden = !atlassian || oauth; siteURL.required = atlassian && !oauth;
      email.parentElement.hidden = !atlassian || oauth;
      token.parentElement.hidden = oauth;
      const provider = sharing.querySelector('option[value="provider"]');
      if (provider) {
        provider.disabled = !["github", "slack", "gdrive"].includes(service.value);
        if (provider.disabled && sharing.value === "provider") sharing.value = "owner";
      }
    };
    const selectedAccount = async () => {
      labels(); sites = []; site.replaceChildren();
      if (!["jira", "confluence"].includes(service.value) || !credential.value.startsWith("oauth:")) return;
      const selected = credential.value;
      const result = await command("oauth.resources", {account_id: selected.slice(6)});
      if (credential.value !== selected) return;
      sites = result.sites;
      for (const value of sites) { const option = el("option", `${value.name} · ${value.url}`); option.value = value.id; site.append(option); }
      if (!sites.length) throw new Error("This Atlassian account has no authorized sites. Reconnect with the required Jira or Confluence scopes.");
    };
    const refill = () => {
      credential.replaceChildren();
      const provider = ["jira", "confluence"].includes(service.value) ? "atlassian" : service.value;
      const choices = [["", "Choose an account, or enter a token below"], ...accounts.accounts.filter(a => a.provider === provider && a.state === "active").map(a => [`oauth:${a.id}`, a.display_name]), ...secrets.map(s => [`secret:${s.id}`, s.name])];
      for (const [value, label] of choices) { const option = el("option", label); option.value = value; credential.append(option); }
      labels();
    };
    driveMode.addEventListener("change", labels); service.addEventListener("change", refill);
    credential.addEventListener("change", () => safe(selectedAccount)); refill();
    form.append(el("p", connectionNotes.join(" "), "muted"));
  }, async values => {
    const atlassian = ["jira", "confluence"].includes(values.kind);
    let reference = values.credential_ref;
    if (values.token && !reference.startsWith("oauth:")) {
      if (atlassian && !values.username) throw new Error("Enter your Atlassian account email with the API token.");
      const saved = atlassian ? JSON.stringify({username: values.username, password: values.token}) : values.token;
      reference = (await command("credential.store", {name: values.name, value: saved})).credential_ref;
    }
    if (!reference) throw new Error("Connect an account or enter a source access token.");
    const config = {acl: {workspace_readers: values.sharing !== "owner", credentials: []}, permission_mode: values.sharing === "provider" ? "provider" : "explicit", sync_every_seconds: Number(values.interval)};
    if (atlassian) {
      if (reference.startsWith("oauth:")) {
        const site = sites.find(s => s.id === values.site_id);
        if (!site) throw new Error("Select an authorized Atlassian site.");
        config.base_url = `https://api.atlassian.com/ex/${values.kind}/${encodeURIComponent(site.id)}`; config.site_url = site.url; config.auth_type = "bearer";
      } else { config.base_url = values.site_url.replace(/\/$/, ""); config.site_url = config.base_url; config.auth_type = "basic"; }
      config[values.kind === "jira" ? "jql" : "space_key"] = values.scope.trim();
    } else if (values.kind === "github") config.repository = values.scope.trim();
    else if (values.kind === "gdrive" && values.drive_mode === "folder") config.folder_id = values.scope.trim();
    else config[({slack: "channels", notion: "page_ids", gdrive: "file_ids"})[values.kind]] = values.scope.split(",").map(v => v.trim()).filter(Boolean);
    if (values.kind === "gdrive") config.recursive = true;
    const connection = await command("connector.add", {name: values.name, kind: values.kind, credential_ref: reference, config});
    await queue("connector", {connector_id: connection.id, max_requests: 64});
    await loadConnections(); notice("Connection added. The first source sync is queued.");
  });
}

const oauthDefaults = {
  github: {authorization_url: "https://github.com/login/oauth/authorize", token_url: "https://github.com/login/oauth/access_token", identity_url: "https://api.github.com/user", scopes: ["repo", "read:org"], pkce: true},
  slack: {authorization_url: "https://slack.com/oauth/v2/authorize", token_url: "https://slack.com/api/oauth.v2/auth", identity_url: "https://slack.com/api/auth.test", scopes: ["channels:read", "channels:history", "groups:read", "groups:history", "users:read"], pkce: false},
  gdrive: {authorization_url: "https://accounts.google.com/o/oauth2/v2/auth", token_url: "https://oauth2.googleapis.com/token", identity_url: "https://openidconnect.googleapis.com/v1/userinfo", scopes: ["openid", "email", "profile", "https://www.googleapis.com/auth/drive.readonly"], pkce: true},
  notion: {authorization_url: "https://api.notion.com/v1/oauth/authorize", token_url: "https://api.notion.com/v1/oauth/token", identity_url: "https://api.notion.com/v1/users/me", scopes: [], pkce: false},
  atlassian: {authorization_url: "https://auth.atlassian.com/authorize", token_url: "https://auth.atlassian.com/oauth/token", identity_url: "https://api.atlassian.com/me", scopes: ["read:jira-work", "read:jira-user", "read:confluence-content.all", "read:confluence-space.summary", "read:me", "offline_access"], pkce: false}
};

async function connectAccount() {
  const status = await command("oauth.status");
  const root = dialog("Connect an account");
  root.append(el("p", "Choose a service to authorize access in your browser."));
  for (const [provider, title] of oauthServices) {
    const item = row(title, status.providers.includes(provider) ? "Ready to connect" : "One-time application setup needed");
    if (status.providers.includes(provider)) item.actions.append(button("Connect", async () => { const result = await api(`${base()}/oauth/start`, {provider}); location.assign(result.authorization_url); }));
    item.actions.append(button("Application setup", () => registerOAuth(provider, title))); root.append(item.node);
  }
  for (const account of status.accounts) {
    const item = row(account.display_name, `${account.provider} · ${account.state}`);
    item.actions.append(button("Disconnect", async () => { await command("oauth.disconnect", {account_id: account.id}); await connectAccount(); await loadConnections(); })); root.append(item.node);
  }
}
function registerOAuth(provider, title) {
  formDialog(`${title} application setup`, "Save application", form => {
    form.append(el("p", "The workspace owner registers an OAuth app once. Teammates can then use the Connect button. Set this exact callback URL in the provider’s application settings:"), el("code", `${location.origin}/oauth/callback/${provider}`));
    field(form, "Application client ID", "client_id");
    field(form, "Authorized scopes", "scopes", {required: false, value: oauthDefaults[provider].scopes.join(" ")});
    const secret = field(form, "Application client secret", "secret", {required: !oauthDefaults[provider].pkce}); secret.type = "password"; secret.autocomplete = "off";
  }, async values => {
    const reference = values.secret ? (await command("credential.store", {name: `${title} OAuth application`, value: values.secret})).credential_ref : null;
    await command("oauth.configure", {provider, config: {...oauthDefaults[provider], scopes: values.scopes.split(/[ ,]+/).filter(Boolean), client_id: values.client_id, client_secret_ref: reference}});
    notice(`${title} is configured. Choose Connect account to authorize it.`);
  });
}

function uploadSource() {
  formDialog("Add documents", "Import documents", form => {
    const input = field(form, "Choose files", "files"); input.type = "file"; input.multiple = true;
    input.accept = ".pdf,.docx,.doc,.pptx,.xlsx,.xls,.xlsb,.ods,.odt,.odp,.epub,.html,.htm,.rtf,.txt,.md,.png,.jpg,.jpeg,.tiff,.webp";
    field(form, "Who can read these documents?", "sharing", {choices: [["owner", "Only me"], ["workspace", "Workspace readers"]]});
    form.append(el("p", "Up to 16 MB per file. Originals are retained with the extracted text. Image recognition requires the Mac document helper.", "muted"));
  }, async values => {
    const files = [...$('#detail-content input[type="file"]').files];
    if (!files.length || files.length > 20) throw new Error("Choose between one and twenty documents.");
    const name = values.sharing === "owner" ? "Private documents" : "Shared documents";
    const existing = (await command("connector.list")).connectors.find(c => c.kind === "jsonl" && c.name === name && c.config.acl.workspace_readers === (values.sharing === "workspace"));
    const connection = existing || await command("connector.add", {name, kind: "jsonl", config: {acl: {workspace_readers: values.sharing === "workspace", credentials: []}}});
    for (const file of files) {
      if (file.size > 16 * 1024 * 1024) throw new Error(`${file.name} exceeds 16 MB.`);
      const data = await new Promise((resolve, reject) => { const reader = new FileReader(); reader.onload = () => resolve(reader.result.split(",")[1]); reader.onerror = reject; reader.readAsDataURL(file); });
      await command("document.import", {connector_id: connection.id, name: file.name, base64: data});
    }
    await refresh(); notice(`${files.length} document${files.length === 1 ? "" : "s"} imported.`);
  });
}

async function manageAccess() {
  const value = await command("workspace.members");
  const root = dialog("Workspace access");
  root.append(el("p", "Give each teammate or agent a separate key. Revoking a key immediately removes its access and cancels its queued work."), button("Add teammate or agent", newMember));
  const members = [];
  for (const member of value.members) {
    const item = row(member.label, `${member.role}${member.revoked_at ? " · revoked" : ""}`, member.expires_at ? `Expires ${new Date(member.expires_at).toLocaleString()}` : undefined);
    if (!member.revoked_at) item.actions.append(button("Revoke", async () => { await api(`${base()}/credentials/${member.id}/revoke`, {}); await manageAccess(); }));
    root.append(item.node); members.push({member, item});
  }
  // An installed edition can add to each member's row (/extensions.js).
  if (typeof decorateAccess === "function") await decorateAccess(root, members);
}
function newMember() {
  formDialog("Add workspace access", "Create access key", form => {
    field(form, "Name", "label");
    field(form, "Role", "role", {choices: [["reader", "Read sources and reviewed memory"], ["agent", "Read, propose memory and run inference"], ["reviewer", "Read, collect sources and approve memory"], ["collector", "Import sources and propose memory"], ["runner", "Run inference and propose memory"]]});
    field(form, "Expires in", "days", {choices: [["30", "30 days"], ["7", "7 days"], ["90", "90 days"]]});
  }, async values => {
    const result = await api(`${base()}/credentials`, {label: values.label, role: values.role, expires_at: new Date(Date.now() + Number(values.days) * 86400000).toISOString()});
    // Show once after the submission dialog closes, never in query strings or localStorage.
    setTimeout(() => { const root = dialog("Save this access key"); root.append(el("p", "Share it privately with this teammate or agent. It will not be shown again."), el("pre", `Workspace: ${state.workspace}\nAccess key: ${result.capability}`)); }, 0);
  });
}
$("#configure-model").addEventListener("click", () => safe(configureModel));
$("#connect-account").addEventListener("click", () => safe(connectAccount));
$("#manage-access").addEventListener("click", () => safe(manageAccess));
$("#upload-source").addEventListener("click", uploadSource);

async function indexMaintenance() {
  const data = await workspaceCommand("projection.status"), root = dialog("External index cleanup");
  root.append(el("p", "Outdated copies are withheld from answers immediately. This queue also removes the copies stored in connected services."));
  const list = el("div", undefined, "list");
  for (const cleanup of data.cleanups) {
    const item = row(`${cleanup.kind === "neo4j" ? "Graph" : "Vector"} index`, cleanup.error || cleanup.reason, `${cleanup.attempts} attempts · ${new Date(cleanup.updated_at).toLocaleString()}`);
    item.main.prepend(tag(cleanup.state));
    if (!["complete", "running"].includes(cleanup.state)) item.actions.append(button("Retry cleanup", async () => {
      await workspaceCommand("projection.retry", {cleanup_id: cleanup.id}); await indexMaintenance();
    }));
    list.append(item.node);
  }
  if (!data.cleanups.length) empty(list, "No cleanup work", "No external index removals have been requested.");
  root.append(list, button("Refresh status", indexMaintenance));
}
$("#index-maintenance").addEventListener("click", () => safe(indexMaintenance));
