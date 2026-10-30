# Break-even costing

What one printed card costs and what to charge for the hardware investment to
pay for itself. All figures in Euro. Back of the envelope: consumables only, no
electricity, printer wear, labour or packaging.

## Fixed investment

| item                        |    € |
|-----------------------------|-----:|
| YMCKOK ribbon + shipping material | 156,91 |
| card printer                | 276,34 |
| shipping label printer      |  25,90 |
| RFID cards, pack of 100     |  28,56 |
| RFID reader (Primacy retrofit) | 10,99 |
| printer cleaning kit        |  44,02 |
| **total**                   | **542,72** |

## Unit cost per single-sided card

| item        | price   | per | cost   |
|-------------|--------:|----:|-------:|
| YMCKO ribbon  | 57,11 | 200 prints | 0,2856 |
| YMCKOK ribbon | 70,20 | 200 prints | 0,3510 |
| RFID card     | 0,40  | piece      | 0,4000 |

                       YMCKO 0,6856   YMCKOK 0,7510

The RFID pack of 100 at 28,56 implies 0,2856 per card instead; 0,40 is the
replenishment price and is the conservative figure used here. On the pack price
the unit costs are 0,5712 (YMCKO) and 0,6366 (YMCKOK).

## Price per card to break even after N cards

`price = unit cost + 542,72 / N`

|    N | fixed/card | YMCKO | YMCKOK | quoted |
|-----:|-----------:|------:|-------:|:-------|
|   50 |      10,85 | 11,54 |  11,61 | 11,55 / 11,65 |
|  100 |       5,43 |  6,11 |   6,18 |  6,15 /  6,20 |
|  200 |       2,71 |  3,40 |   3,46 |  3,40 /  3,50 |
|  500 |       1,09 |  1,77 |   1,84 |  1,80 /  1,85 |

Re-slicing the investment: each 100 € less fixed lowers the 50-card price by
2,00, the 100-card price by 1,00, the 200-card price by 0,50 and the 500-card
price by 0,20. With the whole 156,91 line counted as consumable stock rather
than investment (fixed 385,81) the ladder is 8,40 / 4,54 / 2,61 / 1,46 (YMCKO)
and 8,47 / 4,61 / 2,68 / 1,52 (YMCKOK).

On the cheaper card price (0,2856) it is 11,45 / 6,00 / 3,30 / 1,70 (YMCKO) and
11,50 / 6,10 / 3,40 / 1,75 (YMCKOK).

## Against the shop's seeded price

`ribbon_prices` seeds one price per _billing unit_ at 500 cents (README,
"Setting it up" step 5), and the unit differs per ribbon: YMCKO prices a printed
side, YMCKOK a whole card. A single-sided card therefore charges that price
once. At 5,00 € per card the investment is recovered after ~126 cards on YMCKO
and ~128 on YMCKOK, so the seeded price is already well past break-even at every
volume above.

The 156,91 line bundles YMCKOK ribbon _stock_ with shipping material, so
charging ribbon per card on top of it double counts the prepaid rolls; subtract
them (e.g. one 70,20 roll, fixed 472,52) for the strict figure: 10,14 / 5,41 /
3,05 / 1,63 on YMCKO.

A reprint is the expensive kind of mistake: one wasted card costs a full unit —
0,686 on YMCKO, 0,751 on YMCKOK.
