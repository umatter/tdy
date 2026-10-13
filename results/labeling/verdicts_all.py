# Complete audit verdicts, 148 sampled corpus files.
# flagged set (30) adjudicated first; unflagged set (118) audited with the checklist protocol.
MISREAD = {
 # --- from the crowd-flagged set ---
 'country_subdivisions.csv':('csv','encoding','windows-1252 chosen for valid UTF-8; mojibake',0.80,'UNFLAGGED'),
 'weather.csv':('csv','scale','decimal scale from first 500 rows; 153.5456 -> 153.546',0.45,'noted'),
 'requirements.txt':('other','header-promote','headerless; numpy==1.22.0 absorbed into column name',0.60,'partial'),
 'bouldercreek_09_2013.txt':('other','marker-row','RDB format-descriptor row read as data',0.55,'noted'),
 'h17.xlsx':('excel','stacked','12 stacked sub-tables read as one',0.45,'noted'),
 # --- from the unflagged set ---
 'state_retail.csv':('csv','footer-overfire','trailing row with subsector "total" silently dropped',None,'UNFLAGGED'),
 'hcny_p11_t009.csv':('csv','stacked','NASS h/u/f rows as data; count 65 vs 52',0.50,'noted'),
 'individual_results_df.csv':('csv','probe-window','p7 typed text; values only after row 21513',None,'UNFLAGGED'),
 'Merged Costs (1.0) - Sheet1.csv':('csv','footer-underfire','8 summary rows + 2 blanks as data; 584 vs 574',None,'partial'),
 'SOLUTION_FILES.txt':('other','header-promote','168 paths; line 1 -> column name; count 167',None,'UNFLAGGED'),
 'workflows-version.txt':('other','header-promote','single line 1.0.1 -> column name; 0 rows returned',None,'UNFLAGGED'),
 'h01w.xlsx':('excel','stacked','skip overshot; 2019 data row promoted as header',None,'UNFLAGGED'),
 'ttb_brewery_state_2008-2019.xlsx':('excel','footnote-block','footnotes+total as data; 60 vs 51; col typed text',None,'partial'),
 'ttb_brewery_size_2017.xlsx':('excel','footnote-block','legend/footnotes as data; 27 vs ~13',None,'partial'),
 'ttb_brewery_size_2011.xlsx':('excel','footnote-block','legend/footnotes as data; 26 vs ~12',None,'partial'),
 'ttb_brewery_size_2018.xlsx':('excel','footnote-block','legend block as data; 27 vs 13',0.80,'UNFLAGGED'),
 'supplemental-table5.xlsx':('excel','footnote-block','FOOTNOTES + 3 paragraphs as cities; 182 vs 178',None,'partial'),
 'WealthbyRace.xlsx':('excel','stacked','two stacked tables + prose commingled',None,'noted'),
 'ttb_monthly_stats_2018-12.xlsx':('excel','cell-newline','embedded newline in cell split into 2 rows; 17 vs 11',None,'noted'),
 'bls-2020.xlsx':('excel','stacked','section labels + note as data; 126 vs 115',None,'noted'),
 'PCA_Report_FY16Q3.xlsx':('excel','money-typing','5 of 6 currency cols float64; 255871181.10999995',None,'UNFLAGGED'),
 'h03b.xlsx':('excel','stacked','repeated headers/section labels as data; 163 incl 10 spurious',None,'noted'),
 'h02b.xlsx':('excel','stacked','repeated headers as data; 84 incl header row',None,'noted'),
}
AMBIGUOUS = {'hcny_p11_t013.csv':'csv','ttb_monthly_stats_2019-05.xlsx':'excel','ttb_monthly_stats_2018-01.xlsx':'excel',
             'hcny_p02_t001.csv':'csv','doaj-article-sample.csv':'csv','mama-polar-bear.json':'json'}
DEGRADED = {'hcny_p04_t001.csv':'csv','hcny_p03_t005.csv':'csv',
            'u2-d15-studies-confounding html.json':'json','gracie-bench.json':'json',
            'ae-05-hotels-datatypes/type-coercion html.json':'json','LICENSE.txt (jquery, batch8)':'other',
            '83-hell-or-high-water-film.txt':'other','9-the-godfather.txt':'other',
            '16-casablanca-film.txt':'other','LICENSE.txt (jquery, batch9)':'other',
            '61-the-night-of-the-hunter-film.txt':'other','67-the-dark-knight-film.txt':'other',
            'LICENSE.txt (tidy_data_python)':'other','billboard-degraded-placeholder':'json'}
