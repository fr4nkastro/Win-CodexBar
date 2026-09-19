export const WIDTH = 280;
export const DATE_EDGE_PADDING = 36;
export const BAR_GAP = 2;
export const PLOT_WIDTH = WIDTH - DATE_EDGE_PADDING * 2;
export const AXIS_MAX_X = WIDTH / 2;

export function shouldRenderCenterMax(count: number): boolean {
  return count >= 3;
}

export function getBarWidth(count: number): number {
  const barCount = Math.max(1, count);
  return Math.max(1, (PLOT_WIDTH - (barCount - 1) * BAR_GAP) / barCount);
}

export function getBarX(index: number, count: number): number {
  return DATE_EDGE_PADDING + index * (getBarWidth(count) + BAR_GAP);
}

export function getBarCenter(index: number, count: number): number {
  return getBarX(index, count) + getBarWidth(count) / 2;
}

export interface BarGeometry {
  barWidth: number;
  showCenterMax: boolean;
  x: (index: number) => number;
  center: (index: number) => number;
}

export function getBarGeometry(count: number): BarGeometry {
  const barWidth = getBarWidth(count);
  const x = (index: number) => getBarX(index, count);

  return {
    barWidth,
    showCenterMax: shouldRenderCenterMax(count),
    x,
    center: (index: number) => x(index) + barWidth / 2,
  };
}

export function getLineX(index: number, count: number): number {
  if (count <= 1) return DATE_EDGE_PADDING;
  return DATE_EDGE_PADDING + (index / (count - 1)) * PLOT_WIDTH;
}
