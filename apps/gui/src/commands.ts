import { View } from "./activity";

export type MenuName = "File" | "Edit" | "View" | "Disk" | "Partition" | "Recovery" | "Tools" | "Help";

export const MENUS: MenuName[] = ["File", "Edit", "View", "Disk", "Partition", "Recovery", "Tools", "Help"];

/** What the Back up & clone tab can be asked to show. */
export type BackupTask = "image" | "rescue" | "clone" | "partition";

/** The Hex editor's side panel tabs. */
export type HexPanel = "structure" | "partitions" | "bookmarks";

/** Where to go in a disk's workspace, and what to show or focus there. */
export type Intent = {
  view: View;
  backup?: BackupTask;
  /** For "Copy to another disk": the partition, by its first sector. */
  startLba?: number;
  panel?: HexPanel;
  focus?: "find" | "goto";
};

/** One action, as the menus, the shortcuts and the action search show it. */
export type Command = {
  id: string;
  menu: MenuName;
  /** Items in a menu come in groups, with a line between groups. */
  group: number;
  label: string;
  /** "Ctrl+O", "F5", "Alt+Home". */
  shortcut?: string;
  /** Other words people may search for it by. */
  keywords?: string;
  /** A line on what it does, for the action search. */
  hint?: string;
  /** Why it can't be used right now; it's greyed out while set. */
  unavailable?: string | null;
  danger?: boolean;
  run: () => void;
};

/** What the commands act on. */
export type CommandContext = {
  /** The open disk, volume or image, if any. */
  source: { id: string; title: string; kind: "disk" | "volume" | "image"; windows: boolean } | null;
  elevated: boolean;
  go: (intent: Intent) => void;
  home: () => void;
  openImage: () => void;
  close: () => void;
  exit: () => void;
  restartAsAdmin: () => void;
  refresh: () => void;
  toggleActivity: () => void;
  openSearch: () => void;
  showShortcuts: () => void;
  showAbout: () => void;
};

export const TAB_NAMES: Record<View, string> = {
  overview: "Overview",
  files: "Browse files",
  recover: "Recover data",
  partitions: "Partitions",
  backup: "Back up & clone",
  health: "Health",
  hex: "Hex editor",
};

export const TABS: View[] = ["overview", "files", "recover", "partitions", "backup", "health", "hex"];

/** Every action, in menu order. */
export function buildCommands(ctx: CommandContext): Command[] {
  const s = ctx.source;
  const noSource = s ? null : "Pick a disk, partition or image file on the left first.";
  const notImage = s?.kind === "image" ? "Image files have no drive to check." : noSource;
  const diskOnly =
    s?.kind === "volume" ? "Open the disk this volume is on (above it on the left)." : noSource;
  const onMap = "Opens Partitions: pick the partition or free space on the map, then the action.";
  const go = (intent: Intent) => () => ctx.go(intent);
  const partitions = go({ view: "partitions" });

  const commands: Command[] = [
    // File
    { id: "open-image", menu: "File", group: 0, label: "Open a disk image…", shortcut: "Ctrl+O", keywords: "img dd file", run: ctx.openImage },
    { id: "close", menu: "File", group: 1, label: "Close", shortcut: "Ctrl+W", unavailable: s ? null : "Nothing is open.", run: ctx.close },
    {
      id: "restart-admin",
      menu: "File",
      group: 2,
      label: "Restart as administrator",
      keywords: "elevate admin rights permission",
      unavailable: ctx.elevated ? "DataReco is already running as administrator." : null,
      run: ctx.restartAsAdmin,
    },
    { id: "exit", menu: "File", group: 3, label: "Exit", shortcut: "Alt+F4", keywords: "quit", run: ctx.exit },

    // Edit
    { id: "find", menu: "Edit", group: 0, label: "Find…", shortcut: "Ctrl+F", keywords: "search text hex bytes", unavailable: noSource, run: go({ view: "hex", focus: "find" }) },
    { id: "goto", menu: "Edit", group: 0, label: "Go to sector…", shortcut: "Ctrl+G", keywords: "offset jump", unavailable: noSource, run: go({ view: "hex", focus: "goto" }) },
    { id: "bookmarks", menu: "Edit", group: 1, label: "Bookmarks", shortcut: "Ctrl+B", unavailable: noSource, run: go({ view: "hex", panel: "bookmarks" }) },

    // View
    { id: "home", menu: "View", group: 0, label: "Home", shortcut: "Alt+Home", keywords: "start", run: ctx.home },
    ...TABS.map(
      (view, i): Command => ({
        id: `tab-${view}`,
        menu: "View",
        group: 1,
        label: TAB_NAMES[view],
        shortcut: `Ctrl+${i + 1}`,
        unavailable: noSource,
        run: go({ view }),
      }),
    ),
    { id: "activity", menu: "View", group: 2, label: "Activity", shortcut: "Ctrl+J", keywords: "tasks progress running jobs", run: ctx.toggleActivity },
    { id: "refresh", menu: "View", group: 3, label: "Look for disks again", shortcut: "F5", keywords: "refresh reload devices", run: ctx.refresh },

    // Disk
    { id: "health", menu: "Disk", group: 0, label: "Check health", keywords: "smart s.m.a.r.t. temperature wear", unavailable: notImage, run: go({ view: "health" }) },
    { id: "surface", menu: "Disk", group: 0, label: "Surface test", keywords: "bad sectors scan read test", unavailable: noSource, run: go({ view: "health" }) },
    { id: "speed", menu: "Disk", group: 0, label: "Read speed test", keywords: "benchmark", unavailable: notImage, run: go({ view: "health" }) },
    { id: "backup-image", menu: "Disk", group: 1, label: "Back up to an image…", keywords: "image backup save copy file img", unavailable: noSource, run: go({ view: "backup", backup: "image" }) },
    {
      id: "rescue",
      menu: "Disk",
      group: 1,
      label: s?.kind === "image" ? "Restore this image to a disk…" : "Rescue a failing drive…",
      keywords: "failing bad sectors ddrescue copy restore image clone exact",
      unavailable: noSource,
      run: go({ view: "backup", backup: "rescue" }),
    },
    {
      id: "clone",
      menu: "Disk",
      group: 1,
      label: s?.windows ? "Move Windows to a new disk…" : "Clone to another disk…",
      keywords: "clone migrate copy windows ssd new disk",
      unavailable: diskOnly,
      run: go({ view: "backup", backup: "clone" }),
    },
    { id: "initialize", menu: "Disk", group: 2, label: "Initialize, or convert to GPT or MBR…", keywords: "partition table style", unavailable: diskOnly, run: partitions },
    { id: "wipe-disk", menu: "Disk", group: 2, label: "Wipe the whole disk…", keywords: "erase secure delete", danger: true, unavailable: diskOnly, run: partitions },

    // Partition
    { id: "create", menu: "Partition", group: 0, label: "Create…", hint: onMap, unavailable: noSource, run: partitions },
    { id: "delete", menu: "Partition", group: 0, label: "Delete", hint: onMap, unavailable: noSource, run: partitions },
    { id: "format", menu: "Partition", group: 0, label: "Format…", keywords: "fat32 exfat ntfs", hint: onMap, unavailable: noSource, run: partitions },
    { id: "resize", menu: "Partition", group: 1, label: "Resize…", keywords: "shrink extend grow", hint: onMap, unavailable: noSource, run: partitions },
    { id: "move", menu: "Partition", group: 1, label: "Move…", hint: onMap, unavailable: noSource, run: partitions },
    { id: "align", menu: "Partition", group: 1, label: "Align to 1 MiB…", keywords: "ssd 4k", hint: onMap, unavailable: noSource, run: partitions },
    { id: "copy-partition", menu: "Partition", group: 1, label: "Copy to another disk…", keywords: "clone partition", unavailable: diskOnly, run: go({ view: "backup", backup: "partition" }) },
    { id: "label", menu: "Partition", group: 2, label: "Label and drive letter…", keywords: "rename letter", hint: onMap, unavailable: noSource, run: partitions },
    { id: "type", menu: "Partition", group: 2, label: "Type, name, hide or set active…", hint: onMap, unavailable: noSource, run: partitions },
    { id: "apply", menu: "Partition", group: 3, label: "Review and apply changes…", shortcut: "Ctrl+Enter", keywords: "write pending", unavailable: noSource, run: partitions },
    { id: "undo", menu: "Partition", group: 3, label: "Undo a change…", keywords: "restore backup history", unavailable: noSource, run: partitions },

    // Recovery
    { id: "recover-deleted", menu: "Recovery", group: 0, label: "Recover deleted files…", keywords: "undelete restore files photos", unavailable: noSource, run: go({ view: "files" }) },
    { id: "lost-drives", menu: "Recovery", group: 0, label: "Search for lost drives", keywords: "scan formatted raw lost volume", unavailable: noSource, run: go({ view: "recover" }) },
    { id: "lost-partitions", menu: "Recovery", group: 0, label: "Find lost partitions", keywords: "partition table wiped deleted partition", unavailable: noSource, run: go({ view: "recover" }) },
    { id: "by-type", menu: "Recovery", group: 0, label: "Find files by type", keywords: "raw carve signatures photos jpeg", unavailable: noSource, run: go({ view: "recover" }) },

    // Tools
    { id: "search", menu: "Tools", group: 0, label: "Search actions", shortcut: "Ctrl+K", keywords: "command palette find action", run: ctx.openSearch },
    { id: "structures", menu: "Tools", group: 1, label: "Structure templates", keywords: "decode boot sector mft", unavailable: noSource, run: go({ view: "hex", panel: "structure" }) },

    // Help
    { id: "shortcuts", menu: "Help", group: 0, label: "Keyboard shortcuts", shortcut: "Ctrl+/", keywords: "keys", run: ctx.showShortcuts },
    { id: "about", menu: "Help", group: 1, label: "About DataReco", keywords: "version", run: ctx.showAbout },
  ];
  return commands;
}

/** Whether a key press is the shortcut "Ctrl+Shift+K", "F5", "Alt+Home"… */
export function matches(shortcut: string, event: KeyboardEvent): boolean {
  const parts = shortcut.split("+");
  const key = parts.pop() ?? "";
  const wants = new Set(parts.map((p) => p.toLowerCase()));
  if (wants.has("ctrl") !== event.ctrlKey || wants.has("shift") !== event.shiftKey || wants.has("alt") !== event.altKey) {
    return false;
  }
  return event.key.toLowerCase() === key.toLowerCase();
}

/** Scores how well a command matches what was typed: 0 for no match. */
export function score(command: Command, query: string): number {
  const words = query.toLowerCase().split(/\s+/).filter(Boolean);
  if (words.length === 0) return 1;
  const label = command.label.toLowerCase();
  const all = `${label} ${command.menu.toLowerCase()} ${(command.keywords ?? "").toLowerCase()}`;
  let total = 0;
  for (const word of words) {
    if (label.startsWith(word)) total += 4;
    else if (label.includes(` ${word}`)) total += 3;
    else if (label.includes(word)) total += 2;
    else if (all.includes(word)) total += 1;
    else return 0;
  }
  return total;
}
