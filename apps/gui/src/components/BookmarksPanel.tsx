import { FormEvent, useState } from "react";
import { Bookmark, loadBookmarks, saveBookmarks } from "../bookmarks";
import { formatNumber, hex } from "../format";

type Props = {
  sourceId: string;
  sectorSize: number;
  cursor: number;
  onJump: (offset: number) => void;
};

/** Named places on this source, saved on this computer. */
export function BookmarksPanel({ sourceId, sectorSize, cursor, onJump }: Props) {
  const [bookmarks, setBookmarks] = useState<Bookmark[]>(() => loadBookmarks(sourceId));
  const [name, setName] = useState("");

  function update(next: Bookmark[]) {
    setBookmarks(next);
    saveBookmarks(sourceId, next);
  }

  function add(event: FormEvent) {
    event.preventDefault();
    const label = name.trim() || `Sector ${formatNumber(Math.floor(cursor / sectorSize))}`;
    update([...bookmarks, { name: label, offset: cursor }].sort((a, b) => a.offset - b.offset));
    setName("");
  }

  return (
    <div className="bookmarks-panel">
      <form className="panel-section bookmark-form" onSubmit={add}>
        <input
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder="Name (optional)"
          aria-label="Bookmark name"
        />
        <button type="submit">Bookmark cursor</button>
      </form>
      {bookmarks.length === 0 ? (
        <p className="panel-section muted small">
          No bookmarks yet. They're saved on this computer, separately for each disk and image.
        </p>
      ) : (
        <ul className="bookmark-list">
          {bookmarks.map((bookmark, i) => (
            <li key={`${bookmark.offset}-${i}`}>
              <button className="bookmark" onClick={() => onJump(bookmark.offset)}>
                <span className="bookmark-name">{bookmark.name}</span>
                <span className="muted small">
                  0x{hex(bookmark.offset, 1)} · sector {formatNumber(Math.floor(bookmark.offset / sectorSize))}
                </span>
              </button>
              <button
                className="link-button"
                aria-label={`Remove bookmark ${bookmark.name}`}
                onClick={() => update(bookmarks.filter((_, j) => j !== i))}
              >
                Remove
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
