import { getSlides, loadPresentation } from '@office-kit/pptx';
import { renderSlideToSvg } from '@office-kit/pptx-preview';

// 按需加载、按页渲染，避免大 PPTX 一次生成所有幻灯片 DOM。
window.KtreePptx = {
  async open(bytes) {
    const presentation = await loadPresentation(new Uint8Array(bytes));
    const slides = getSlides(presentation);
    return {
      count: slides.length,
      render(index) {
        if (!Number.isInteger(index) || index < 0 || index >= slides.length) {
          throw new RangeError('幻灯片页码超出范围');
        }
        return renderSlideToSvg(presentation, slides[index]);
      },
    };
  },
};
