import { useEffect, useRef, useState } from "react";
import type { KeyboardEvent } from "react";
import type { LibraryItem } from "../../services/mediaCatalog";
import { LibraryTile } from "./LibraryTile";

interface LibraryGridProps {
    items: LibraryItem[];
    onPlay: (item: LibraryItem) => void;
    onMakePermanent: (id: string) => void;
    onDelete: (id: string) => void;
}

/**
 * The tile grid. Keyboard model: the grid is one Tab stop (a roving tabindex);
 * Arrow keys move between tiles, Home/End jump to the first/last, and Tab then
 * moves on through the focused tile's buttons.
 */
export function LibraryGrid({ items, onPlay, onMakePermanent, onDelete }: LibraryGridProps): JSX.Element {
    const [activeIndex, setActiveIndex] = useState(0);
    const tileRefs = useRef<Array<HTMLElement | null>>([]);
    const focusAfterRender = useRef(false);

    // Keep the roving index valid when items are removed or filtered away.
    useEffect(() => {
        if (activeIndex > items.length - 1) setActiveIndex(Math.max(0, items.length - 1));
    }, [items.length, activeIndex]);

    useEffect(() => {
        if (focusAfterRender.current) {
            focusAfterRender.current = false;
            tileRefs.current[activeIndex]?.focus();
        }
    }, [activeIndex]);

    /** Tiles per row, measured from layout (the grid is responsive). */
    function columns(): number {
        const first = tileRefs.current[0];
        if (!first) return 1;
        const top = first.getBoundingClientRect().top;
        let n = 0;
        for (const el of tileRefs.current) {
            if (!el) break;
            if (Math.abs(el.getBoundingClientRect().top - top) < 2) n++;
            else break;
        }
        return Math.max(1, n);
    }

    function onKeyDown(index: number, e: KeyboardEvent<HTMLElement>): void {
        // Only react to keys pressed on the tile itself, never on its buttons.
        if (e.target !== e.currentTarget) return;
        const last = items.length - 1;
        let next: number;
        switch (e.key) {
            case "ArrowRight":
                next = Math.min(last, index + 1);
                break;
            case "ArrowLeft":
                next = Math.max(0, index - 1);
                break;
            case "ArrowDown":
                next = Math.min(last, index + columns());
                break;
            case "ArrowUp":
                next = Math.max(0, index - columns());
                break;
            case "Home":
                next = 0;
                break;
            case "End":
                next = last;
                break;
            default:
                return;
        }
        e.preventDefault();
        if (next === activeIndex) {
            tileRefs.current[next]?.focus();
        } else {
            focusAfterRender.current = true;
            setActiveIndex(next);
        }
    }

    return (
        <ul className="library-grid" role="list" aria-label="Media library" data-testid="library-grid">
            {items.map((item, index) => (
                <LibraryTile
                    key={item.id}
                    ref={(el) => {
                        tileRefs.current[index] = el;
                    }}
                    item={item}
                    active={index === activeIndex}
                    onFocusTile={() => setActiveIndex(index)}
                    onKeyDown={(e) => onKeyDown(index, e)}
                    onPlay={onPlay}
                    onMakePermanent={onMakePermanent}
                    onDelete={onDelete}
                />
            ))}
        </ul>
    );
}
