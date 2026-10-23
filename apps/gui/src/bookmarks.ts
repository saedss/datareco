// Bookmarks per source, kept in this computer's browser storage.

export type Bookmark = { name: string; offset: number };

const key = (sourceId: string) => `datareco.bookmarks.${sourceId}`;

export function loadBookmarks(sourceId: string): Bookmark[] {
  try {
    const parsed: unknown = JSON.parse(localStorage.getItem(key(sourceId)) ?? "[]");
    if (!Array.isArray(parsed)) return [];
    return parsed.filter(
      (b): b is Bookmark => typeof b?.name === "string" && Number.isFinite(b?.offset),
    );
  } catch {
    return [];
  }
}

export function saveBookmarks(sourceId: string, bookmarks: Bookmark[]) {
  try {
    localStorage.setItem(key(sourceId), JSON.stringify(bookmarks));
  } catch {
    // Storage can be unavailable; bookmarks then last until the app closes.
  }
}
