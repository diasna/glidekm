import {
  ButtonItem,
  Field,
  PanelSection,
  PanelSectionRow,
  TextField,
  ToggleField,
  staticClasses,
} from "@decky/ui";
import { callable, definePlugin, toaster } from "@decky/api";
import { useEffect, useState } from "react";
import { FaMousePointer } from "react-icons/fa";

interface PluginState {
  enabled: boolean;
  running: boolean;
  passphrase_set: boolean;
  port: number;
  width: number;
  height: number;
  connected: boolean;
  server: string;
  active: boolean;
  capture: boolean;
  screen: string;
  ip: string;
  error: string;
}

const getState = callable<[], PluginState>("get_state");
const setEnabled = callable<[boolean], PluginState>("set_enabled");
const saveSettings = callable<[string, number, number, number], PluginState>("save_settings");

function Content() {
  const [st, setSt] = useState<PluginState | null>(null);
  const [pass, setPass] = useState("");
  const [port, setPort] = useState("4242");
  const [size, setSize] = useState("");

  const refresh = async () => setSt(await getState());

  useEffect(() => {
    getState().then((s) => {
      setSt(s);
      setPort(String(s.port));
      setSize(s.width && s.height ? `${s.width}x${s.height}` : "");
    });
    const t = setInterval(refresh, 1000);
    return () => clearInterval(t);
  }, []);

  if (!st) return <PanelSection>Loading…</PanelSection>;

  const status = !st.running
    ? st.error || "Stopped"
    : st.connected
      ? st.active
        ? st.capture
          ? "In use (game mode)"
          : "In use"
        : `Connected to ${st.server}`
      : "Waiting for PC…";

  const onSave = async () => {
    const m = size.trim().match(/^(\d+)\s*x\s*(\d+)$/i);
    const [w, h] = m ? [parseInt(m[1]), parseInt(m[2])] : [0, 0];
    const s = await saveSettings(pass, parseInt(port) || 4242, w, h);
    setSt(s);
    setPass("");
    toaster.toast({ title: "glidekm", body: "Settings saved" });
  };

  return (
    <>
      <PanelSection title="Status">
        <PanelSectionRow>
          <ToggleField
            label="Enabled"
            checked={st.enabled}
            onChange={async (v) => setSt(await setEnabled(v))}
          />
        </PanelSectionRow>
        <PanelSectionRow>
          <Field label="State" focusable>
            {status}
          </Field>
        </PanelSectionRow>
        <PanelSectionRow>
          <Field label="This Deck" focusable>
            {st.ip ? `${st.ip}:${st.port}` : "no network"}
          </Field>
        </PanelSectionRow>
        <PanelSectionRow>
          <Field label="Screen" focusable>
            {st.screen || "auto"}
          </Field>
        </PanelSectionRow>
      </PanelSection>
      <PanelSection title="Settings">
        <PanelSectionRow>
          <TextField
            label={st.passphrase_set ? "Passphrase (set — type to change)" : "Passphrase (optional)"}
            value={pass}
            bIsPassword
            onChange={(e) => setPass(e.target.value)}
          />
        </PanelSectionRow>
        <PanelSectionRow>
          <TextField label="UDP port" value={port} onChange={(e) => setPort(e.target.value)} />
        </PanelSectionRow>
        <PanelSectionRow>
          <TextField
            label="Screen size override (e.g. 1280x800, empty = auto)"
            value={size}
            onChange={(e) => setSize(e.target.value)}
          />
        </PanelSectionRow>
        <PanelSectionRow>
          <ButtonItem layout="below" onClick={onSave}>
            Save
          </ButtonItem>
        </PanelSectionRow>
      </PanelSection>
      <PanelSection title="Hotkeys (on the PC)">
        <PanelSectionRow>
          <Field label="Switch PC ⇄ Deck">Ctrl+Alt+Shift+S</Field>
        </PanelSectionRow>
        <PanelSectionRow>
          <Field label="Game mouse (capture)">Ctrl+Alt+Shift+M</Field>
        </PanelSectionRow>
      </PanelSection>
    </>
  );
}

export default definePlugin(() => ({
  name: "glidekm",
  titleView: <div className={staticClasses.Title}>glidekm</div>,
  content: <Content />,
  icon: <FaMousePointer />,
}));
