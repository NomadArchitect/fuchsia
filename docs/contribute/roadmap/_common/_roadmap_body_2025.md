{% include "docs/contribute/roadmap/_common/_list_header.md" %}
    {%- for area in areas|sort %}
    {%- set found=false %}
    {%- for item in roadmap_2025 %}
          {%- if item.area == area %}
            {%- set found=true %}
          {%- endif %}
    {%- endfor %}
    {%- if found %}
  <li class="list-areas"><h3 class="add-link">{{ area }}</h3>
    {%- for item in roadmap_2025 %}
      {%- if item.area == area %}
      <ul class="list">
      <!--Use the same template from 2023-->
        {% include "docs/contribute/roadmap/_common/_list_body_2023.md" %}
      </ul>
      {%- endif %}
    {%- endfor %}
    </li>
    {%- endif %}
  {%- endfor %}
  {% include "docs/contribute/roadmap/_common/_list_footer.md" %}
</div>
