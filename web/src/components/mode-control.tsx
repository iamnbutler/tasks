import { Square, Pause, Play } from "lucide-react";
import { Button } from "@/components/ui/button";
import { useAppState } from "@/hooks/use-app-state";
import { setMode } from "@/lib/api";
import type { Mode } from "@/lib/types";

const modes: { mode: Mode; icon: typeof Square; label: string; activeClass: string }[] = [
  { mode: "stop", icon: Square, label: "Stop", activeClass: "bg-red-600 text-white hover:bg-red-700" },
  { mode: "pause", icon: Pause, label: "Pause", activeClass: "bg-yellow-600 text-white hover:bg-yellow-700" },
  { mode: "play", icon: Play, label: "Play", activeClass: "bg-green-600 text-white hover:bg-green-700" },
];

export function ModeControl() {
  const { snapshot, refreshSnapshot } = useAppState();
  const currentMode = snapshot?.mode;

  async function handleClick(mode: Mode) {
    await setMode(mode);
    refreshSnapshot();
  }

  return (
    <div className="flex items-center gap-0.5">
      {modes.map(({ mode, icon: Icon, label, activeClass }) => {
        const isActive = currentMode === mode;
        return (
          <Button
            key={mode}
            variant="ghost"
            size="sm"
            aria-label={label}
            className={isActive ? activeClass : "text-muted-foreground hover:text-foreground"}
            onClick={() => handleClick(mode)}
          >
            <Icon className="h-4 w-4" />
          </Button>
        );
      })}
    </div>
  );
}
