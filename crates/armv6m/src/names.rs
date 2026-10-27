//! What an address is on the chip, by its memory map: the memory or
//! peripheral it is in, by the name the reference manual gives it, and how
//! far into that. For a person reading an address — `I2C2 + 0x24`, which
//! is RXDR, or `AXI SRAM + 0xd418` — not for the bus, which decodes for
//! itself.
//!
//! Every peripheral the manuals map is named, modelled or not, since an
//! address in one is worth naming either way. The STM32H743/753's are from
//! RM0433, the STM32G0's from RM0444, and the Cortex-M core's own from the
//! ARMv6-M and ARMv7-M architecture manuals.

use crate::bus::Soc;

/// The block `address` is in: its name and its base.
pub fn block(soc: &Soc, address: u32) -> Option<(&'static str, u32)> {
    let table: &[(&str, u32, u32)] = match soc {
        Soc::H7(_) => H7,
        Soc::G0(_) => G0,
    };
    table
        .iter()
        .chain(CORE)
        .find(|&&(_, base, size)| address >= base && address - base < size)
        .map(|&(name, base, _)| (name, base))
}

/// The data register `address` is, if it is one — what a peripheral hands
/// the core a byte from the outside through: `I2C2 RXDR`, `SPI1 RXDR`,
/// `USART3 RDR`. A read of one is where a value comes in.
pub fn data_register(soc: &Soc, address: u32) -> Option<String> {
    let (name, base) = block(soc, address)?;
    let offset = address - base;
    let h7 = matches!(soc, Soc::H7(_));
    let register = if name.starts_with("I2C") && offset == 0x24 {
        "RXDR"
    } else if name.starts_with("SPI") && offset == if h7 { 0x30 } else { 0x0C } {
        if h7 { "RXDR" } else { "DR" }
    } else if (name.starts_with("USART") || name.starts_with("UART") || name.starts_with("LPUART")) && offset == 0x24 {
        "RDR"
    } else {
        return None;
    };
    Some(format!("{name} {register}"))
}

const K: u32 = 1024;

/// The Cortex-M core's own: the System Control Space, by block.
const CORE: &[(&str, u32, u32)] = &[
    ("ITM", 0xE000_0000, 4 * K),
    ("DWT", 0xE000_1000, 4 * K),
    ("FPB", 0xE000_2000, 4 * K),
    ("SysTick", 0xE000_E010, 0x10),
    ("NVIC", 0xE000_E100, 0xC00),
    ("SCB", 0xE000_ED00, 0x90),
    ("MPU", 0xE000_ED90, 0x70),
    ("debug", 0xE000_EDF0, 0x10),
    ("SCS", 0xE000_E000, 4 * K),
];

/// RM0433's memory map: the memories, then each bus's peripherals.
const H7: &[(&str, u32, u32)] = &[
    ("ITCM", 0x0000_0000, 64 * K),
    ("flash bank 1", 0x0800_0000, 1024 * K),
    ("flash bank 2", 0x0810_0000, 1024 * K),
    ("system memory", 0x1FF0_0000, 128 * K),
    ("DTCM", 0x2000_0000, 128 * K),
    ("AXI SRAM", 0x2400_0000, 512 * K),
    ("SRAM1", 0x3000_0000, 128 * K),
    ("SRAM2", 0x3002_0000, 128 * K),
    ("SRAM3", 0x3004_0000, 32 * K),
    ("SRAM4", 0x3800_0000, 64 * K),
    ("backup SRAM", 0x3880_0000, 4 * K),
    // APB1
    ("TIM2", 0x4000_0000, K),
    ("TIM3", 0x4000_0400, K),
    ("TIM4", 0x4000_0800, K),
    ("TIM5", 0x4000_0C00, K),
    ("TIM6", 0x4000_1000, K),
    ("TIM7", 0x4000_1400, K),
    ("TIM12", 0x4000_1800, K),
    ("TIM13", 0x4000_1C00, K),
    ("TIM14", 0x4000_2000, K),
    ("LPTIM1", 0x4000_2400, K),
    ("SPI2", 0x4000_3800, K),
    ("SPI3", 0x4000_3C00, K),
    ("SPDIFRX", 0x4000_4000, K),
    ("USART2", 0x4000_4400, K),
    ("USART3", 0x4000_4800, K),
    ("UART4", 0x4000_4C00, K),
    ("UART5", 0x4000_5000, K),
    ("I2C1", 0x4000_5400, K),
    ("I2C2", 0x4000_5800, K),
    ("I2C3", 0x4000_5C00, K),
    ("HDMI-CEC", 0x4000_6C00, K),
    ("DAC", 0x4000_7400, K),
    ("UART7", 0x4000_7800, K),
    ("UART8", 0x4000_7C00, K),
    ("CRS", 0x4000_8400, K),
    ("SWPMI", 0x4000_8800, K),
    ("OPAMP", 0x4000_9000, K),
    ("MDIOS", 0x4000_9400, K),
    ("FDCAN", 0x4000_A000, 10 * K),
    // APB2
    ("TIM1", 0x4001_0000, K),
    ("TIM8", 0x4001_0400, K),
    ("USART1", 0x4001_1000, K),
    ("USART6", 0x4001_1400, K),
    ("SPI1", 0x4001_3000, K),
    ("SPI4", 0x4001_3400, K),
    ("TIM15", 0x4001_4000, K),
    ("TIM16", 0x4001_4400, K),
    ("TIM17", 0x4001_4800, K),
    ("SPI5", 0x4001_5000, K),
    ("SAI1", 0x4001_5800, K),
    ("SAI2", 0x4001_5C00, K),
    ("SAI3", 0x4001_6000, K),
    ("DFSDM1", 0x4001_7000, K),
    ("HRTIM", 0x4001_7400, K),
    // AHB1
    ("DMA1", 0x4002_0000, K),
    ("DMA2", 0x4002_0400, K),
    ("DMAMUX1", 0x4002_0800, K),
    ("ADC1/2", 0x4002_2000, K),
    ("ART", 0x4002_4400, K),
    ("ETH", 0x4002_8000, 8 * K),
    ("USB1 OTG HS", 0x4004_0000, 256 * K),
    ("USB2 OTG FS", 0x4008_0000, 256 * K),
    // AHB2
    ("DCMI", 0x4802_0000, K),
    ("CRYP", 0x4802_1000, K),
    ("HASH", 0x4802_1400, K),
    ("RNG", 0x4802_1800, K),
    ("SDMMC2", 0x4802_2400, K),
    // APB3 and AHB3
    ("LTDC", 0x5000_1000, 4 * K),
    ("WWDG1", 0x5000_3000, K),
    ("MDMA", 0x5200_0000, 4 * K),
    ("DMA2D", 0x5200_1000, 4 * K),
    ("FLASH", 0x5200_2000, 4 * K),
    ("JPEG", 0x5200_3000, 4 * K),
    ("FMC", 0x5200_4000, 4 * K),
    ("QUADSPI", 0x5200_5000, 4 * K),
    ("SDMMC1", 0x5200_7000, 4 * K),
    ("RAMECC1", 0x5200_9000, 4 * K),
    // APB4
    ("EXTI", 0x5800_0000, K),
    ("SYSCFG", 0x5800_0400, K),
    ("LPUART1", 0x5800_0C00, K),
    ("SPI6", 0x5800_1400, K),
    ("I2C4", 0x5800_1C00, K),
    ("LPTIM2", 0x5800_2400, K),
    ("LPTIM3", 0x5800_2800, K),
    ("LPTIM4", 0x5800_2C00, K),
    ("LPTIM5", 0x5800_3000, K),
    ("COMP1/2", 0x5800_3800, K),
    ("VREFBUF", 0x5800_3C00, K),
    ("RTC", 0x5800_4000, K),
    ("IWDG1", 0x5800_4800, K),
    ("SAI4", 0x5800_5400, K),
    // AHB4
    ("GPIOA", 0x5802_0000, K),
    ("GPIOB", 0x5802_0400, K),
    ("GPIOC", 0x5802_0800, K),
    ("GPIOD", 0x5802_0C00, K),
    ("GPIOE", 0x5802_1000, K),
    ("GPIOF", 0x5802_1400, K),
    ("GPIOG", 0x5802_1800, K),
    ("GPIOH", 0x5802_1C00, K),
    ("GPIOI", 0x5802_2000, K),
    ("GPIOJ", 0x5802_2400, K),
    ("GPIOK", 0x5802_2800, K),
    ("RCC", 0x5802_4400, K),
    ("PWR", 0x5802_4800, K),
    ("CRC", 0x5802_4C00, K),
    ("BDMA", 0x5802_5400, K),
    ("DMAMUX2", 0x5802_5800, K),
    ("ADC3", 0x5802_6000, K),
    ("HSEM", 0x5802_6400, K),
    ("RAMECC3", 0x5802_7000, K),
    ("DBGMCU", 0x5C00_1000, 4 * K),
    ("DBGMCU", 0xE00E_1000, 4 * K),
];

/// RM0444's memory map, for the G0s the bench has: the memories, then each
/// bus's peripherals.
const G0: &[(&str, u32, u32)] = &[
    ("flash", 0x0800_0000, 512 * K),
    ("system memory", 0x1FFF_0000, 30 * K),
    ("option bytes", 0x1FFF_7800, 2 * K),
    ("SRAM", 0x2000_0000, 144 * K),
    // APB
    ("TIM2", 0x4000_0000, K),
    ("TIM3", 0x4000_0400, K),
    ("TIM4", 0x4000_0800, K),
    ("TIM6", 0x4000_1000, K),
    ("TIM7", 0x4000_1400, K),
    ("TIM14", 0x4000_2000, K),
    ("RTC", 0x4000_2800, K),
    ("WWDG", 0x4000_2C00, K),
    ("IWDG", 0x4000_3000, K),
    ("SPI2", 0x4000_3800, K),
    ("SPI3", 0x4000_3C00, K),
    ("USART2", 0x4000_4400, K),
    ("USART3", 0x4000_4800, K),
    ("USART4", 0x4000_4C00, K),
    ("USART5", 0x4000_5000, K),
    ("I2C1", 0x4000_5400, K),
    ("I2C2", 0x4000_5800, K),
    ("I2C3", 0x4000_8800, K),
    ("PWR", 0x4000_7000, K),
    ("DAC", 0x4000_7400, K),
    ("LPTIM1", 0x4000_7C00, K),
    ("LPUART1", 0x4000_8000, K),
    ("LPTIM2", 0x4000_9400, K),
    ("TAMP", 0x4000_B000, K),
    ("SYSCFG", 0x4001_0000, K),
    ("ADC", 0x4001_2400, K),
    ("TIM1", 0x4001_2C00, K),
    ("SPI1", 0x4001_3000, K),
    ("USART1", 0x4001_3800, K),
    ("USART6", 0x4001_3C00, K),
    ("TIM15", 0x4001_4000, K),
    ("TIM16", 0x4001_4400, K),
    ("TIM17", 0x4001_4800, K),
    ("DBG", 0x4001_5800, K),
    // AHB
    ("DMA1", 0x4002_0000, K),
    ("DMA2", 0x4002_0400, K),
    ("DMAMUX", 0x4002_0800, K),
    ("RCC", 0x4002_1000, K),
    ("EXTI", 0x4002_1800, K),
    ("FLASH", 0x4002_2000, K),
    ("CRC", 0x4002_3000, K),
    ("RNG", 0x4002_5000, K),
    ("AES", 0x4002_6000, K),
    // IOPORT
    ("GPIOA", 0x5000_0000, K),
    ("GPIOB", 0x5000_0400, K),
    ("GPIOC", 0x5000_0800, K),
    ("GPIOD", 0x5000_0C00, K),
    ("GPIOE", 0x5000_1000, K),
    ("GPIOF", 0x5000_1400, K),
];

#[cfg(test)]
mod tests {
    use super::block;
    use crate::Machine;

    #[test]
    fn an_h7_address_is_named_by_its_block() {
        let machine = Machine::new(crate::part::Part::named("stm32h753").unwrap(), &[0u8; 8]).unwrap();
        let soc = &machine.bus.soc;
        assert_eq!(block(soc, 0x4000_5824), Some(("I2C2", 0x4000_5800)));
        assert_eq!(block(soc, 0x2400_d418), Some(("AXI SRAM", 0x2400_0000)));
        assert_eq!(block(soc, 0xE000_ED08), Some(("SCB", 0xE000_ED00)));
        assert_eq!(block(soc, 0x6000_0000), None);
    }
}
