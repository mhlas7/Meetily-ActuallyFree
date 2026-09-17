'use client';

import { forwardRef, ComponentProps } from 'react';
import { Button } from '@/components/ui/button';
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from '@/components/ui/tooltip';

/** Keep icon-only toolbar actions named and explain them on keyboard focus. */
export const ToolbarButton = forwardRef<HTMLButtonElement, ComponentProps<typeof Button>>(
  ({ title, ...props }, ref) => {
    const button = <Button {...props} ref={ref} aria-label={props['aria-label'] ?? title} />;
    if (!title) return button;
    return (
      <TooltipProvider delayDuration={250}>
        <Tooltip>
          <TooltipTrigger asChild>{button}</TooltipTrigger>
          <TooltipContent side="bottom">{title}</TooltipContent>
        </Tooltip>
      </TooltipProvider>
    );
  },
);
ToolbarButton.displayName = 'ToolbarButton';
